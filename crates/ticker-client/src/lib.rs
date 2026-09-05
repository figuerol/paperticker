//! Synchronous IPC client for `tickerd`. One persistent connection over a
//! Unix domain socket; one JSON request per line, one JSON response back.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use ticker_proto::{Request, Response};

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        let writer = stream.try_clone()?;
        Ok(Self { reader: BufReader::new(stream), writer })
    }

    pub fn call(&mut self, req: &Request) -> Result<Response> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(anyhow!("daemon closed the connection"));
        }
        let resp: Response = serde_json::from_str(buf.trim_end())
            .with_context(|| format!("parsing daemon reply: {buf:?}"))?;
        Ok(resp)
    }
}
