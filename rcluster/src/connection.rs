use buffered::BUFFER_SIZE;
use errors::{ClusterError, ClusterFuture};
use futures::{Future, future};
use num::FromPrimitive;
use rand::{self, RngCore};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::io::{self as async_io, ReadHalf, WriteHalf};

use std::io::{BufReader, BufWriter};

/// Length of the random separator used in a connection for boundaries.
///
/// **Note: This should always be >= 8.** Values less than "8" may lead to
/// undefined behavior while transferring file content from stream.
pub const MAGIC_LENGTH: usize = 16;

enum_from_primitive! {
    /// Different flags which represent the goal of the request/response.
    #[repr(u8)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum ConnectionFlag {
        MasterPing,
        SlaveOk,
        MasterWantsPath,
        MasterSendsPath,
        MasterWantsExecution,
    }
}

impl Into<u8> for ConnectionFlag {
    fn into(self) -> u8 { self as u8 }
}

/// A connection containing the read and write halves of a TCP stream.
pub type StreamingConnection<S> = Connection<ReadHalf<S>, WriteHalf<S>>;
/// Deconstructed version of a connection. This exists so that we can deconstruct
/// the struct, pass the necessary values for executing a future and reconstruct it back.
pub type ConnectionParts<R, W> = (BufReader<R>, BufWriter<W>, [u8; MAGIC_LENGTH]);

/// Represents a connection (for master/slave). This is called immediately after
/// `connect_async` or `accept_async` (from TLS). All methods of this struct resolve
/// to a future, and so they all can be chained.
pub struct Connection<R: AsyncRead, W: AsyncWrite> {
    reader: BufReader<R>,
    writer: BufWriter<W>,
    magic: [u8; MAGIC_LENGTH],
}

impl<R, W> From<ConnectionParts<R, W>> for Connection<R, W>
    where R: AsyncRead, W: AsyncWrite
{
    fn from(v: ConnectionParts<R, W>) -> Self {
        Connection {
            reader: v.0,
            writer: v.1,
            magic: v.2,
        }
    }
}

impl<R, W> Into<ConnectionParts<R, W>> for Connection<R, W>
    where R: AsyncRead, W: AsyncWrite
{
    #[inline]
    fn into(self) -> ConnectionParts<R, W> {
        (self.reader, self.writer, self.magic)
    }
}

impl<S> StreamingConnection<S>
    where S: AsyncRead + AsyncWrite + 'static
{
    /// Create a connection object for an incoming/outgoing stream. If the `bool` is set
    /// to `true`, then this assumes that the connection is incoming and expects a
    /// a set of bytes (which I call "magic") which begins the connection. If it's `false`,
    /// then this assumes that the connection is outgoing, and so it writes the "magic" bytes.
    pub fn create_for_stream(stream: S, expect_magic: bool) -> ClusterFuture<Self> {
        let (r, w) = stream.split();
        let (reader, writer) = (BufReader::with_capacity(BUFFER_SIZE, r),
                                BufWriter::with_capacity(BUFFER_SIZE, w));
        let mut magic = [0; MAGIC_LENGTH];

        if expect_magic {
            Connection { reader, writer, magic }.read_magic()
        } else {
            let mut rng = rand::thread_rng();
            rng.fill_bytes(&mut magic);
            Connection { reader, writer, magic }.write_magic()
        }
    }
}

impl<R, W> Connection<R, W>
    where R: AsyncRead + 'static, W: AsyncWrite + 'static
{
    /// Write bytes to the "writable half" of this connection and flush the stream.
    #[inline]
    pub fn write_bytes<B>(self, bytes: B) -> ClusterFuture<Self>
        where B: AsRef<[u8]> + 'static
    {
        let (r, w, m) = self.into();
        let async_write = async_io::write_all(w, bytes)
            .and_then(|(w, _)| async_io::flush(w))
            .map(move |w| Connection::from((r, w, m)))
            .map_err(ClusterError::from);
        Box::new(async_write) as ClusterFuture<Self>
    }

    /// Read the magic bytes from this connection. Note that this changes
    /// the magic bytes that already exist in `self` (because we use only one
    /// set of bytes throughout a connection).
    #[inline]
    pub fn read_magic(self) -> ClusterFuture<Self> {
        let (reader, writer, _) = self.into();
        let async_read = async_io::read_exact(reader, [0; MAGIC_LENGTH])
            .map(|(reader, magic)| Connection { reader, writer, magic })
            .map_err(ClusterError::from);
        Box::new(async_read) as ClusterFuture<Self>
    }

    /// Write the magic to this connection's stream.
    #[inline]
    pub fn write_magic(self) -> ClusterFuture<Self> {
        let m = self.magic;
        self.write_bytes(m)
    }

    /// Read flag from this stream. Essentially, a flag is just a byte,
    /// and so if it fails, this will return a future that resolves to an error.
    pub fn read_flag<F>(self) -> ClusterFuture<(Self, ConnectionFlag)>
        where F: FromPrimitive
    {
        let (r, w, m) = self.into();
        let async_handle = async_io::read_exact(r, [0; 1])
            .map_err(ClusterError::from)
            .and_then(move |(r, flag_byte)| {
                let flag = ConnectionFlag::from_u8(flag_byte[0])
                                          .ok_or(ClusterError::UnknownFlag);
                flag.map(move |f| ((r, w, m).into(), f))
            });
        Box::new(async_handle) as ClusterFuture<(Self, ConnectionFlag)>
    }

    /// Write the given flag to this stream.
    #[inline]
    pub fn write_flag<F>(self, flag: F) -> ClusterFuture<Self>
        where F: Into<u8>
    {
        let flag: [u8; 1] = [flag.into()];
        self.write_bytes(flag)
    }

    fn buffered_file_write(self) -> ClusterFuture<Self> {
        let (r, w, m) = self.into();
        let async_handle = async_io::read_until(r, b'\n', Vec::new())
            .map_err(ClusterError::from)
            .and_then(move |(r, bytes)| {
                let path_str = String::from_utf8_lossy(&bytes[..bytes.len() - 1]);
                StreamingBuffer::stream_to_file(r, &m, &*path_str)
                                .and_then(|s| s.stream())
                                .map(move |(r, _fd)| (r, w, m))
            }).and_then(|(r, w, m)| {
                Connection::from((r, w, m)).write_flag(ConnectionFlag::SlaveOk)
            });

        Box::new(async_handle) as ClusterFuture<Self>
    }

    /// The next byte in the `IncomingStream` is a flag. Read it and use
    /// appropriate methods to handle it. This is meant for the slave.
    #[inline]
    pub fn handle_flags(self) -> ClusterFuture<Self> {
        let async_handle = self.read_flag::<ConnectionFlag>().and_then(|(conn, flag)| {
            conn.write_magic().and_then(move |conn| match flag {
                ConnectionFlag::MasterPing => conn.write_flag(ConnectionFlag::SlaveOk),
                _ => {
                    error!("Dunno how to handle {:?}", flag);
                    Box::new(future::ok(conn)) as ClusterFuture<Self>
                },
            })
        });

        Box::new(async_handle) as ClusterFuture<Self>
    }
}
