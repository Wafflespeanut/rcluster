use ProcessType;
use std::io::{BufReader, BufWriter, Read};
use std::net::TcpStream;

pub struct Cluster {
    addrs: Vec<String>,     // FIXME: Change addrs to ToSocketAddr impls
}

impl Cluster {
    pub fn new() -> Cluster {
        Cluster {
            addrs: Vec::new(),
        }
    }

    pub fn add_node(&mut self, addr: &str) -> Result<(), String> {
        let _ = self.ping_addr(addr)?;
        self.addrs.push(addr.to_owned());
        Ok(())
    }

    pub fn ping_addr(&self, addr: &str) -> Result<(), String> {
        let stream = TcpStream::connect(&addr)
                               .map_err(|e| format!("Cannot connect to {} ({})", addr, e))?;
        {
            let mut writer = BufWriter::new(&stream);
            let proc_type = ProcessType::Ping;
            proc_type.into_stream(&mut writer).map_err(|e| format!("Cannot ping {} ({})", addr, e))?;
        }

        let mut reader = BufReader::new(stream);
        let mut response = [0; 1];
        let _ = reader.read_exact(&mut response);
        if response[0] > 0 {
            Ok(())
        } else {
            Err(format!("Failure receiving message from address: {}", addr))
        }
    }

    #[inline]
    pub fn ping_all(&self) -> Result<(), String> {
        for addr in &self.addrs {
            self.ping_addr(addr)?;
        }

        Ok(())
    }
}
