use std::time::Duration;

use crate::channel::ChannelConfig;
use crate::error::{Error, Result};

pub async fn connect(name: &str, config: ChannelConfig) -> Result<super::ShmemConnection> {
    Ok(super::ShmemConnection::from_sync(
        imp::connect_to(name, config).await?,
    ))
}

pub struct ShmemListener(imp::Listener);

impl ShmemListener {
    pub async fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
        Ok(Self(imp::Listener::bind(name, config).await?))
    }

    pub async fn accept(&mut self) -> Result<super::ShmemConnection> {
        Ok(super::ShmemConnection::from_sync(self.0.accept().await?))
    }

    pub async fn accept_timeout(&mut self, timeout: Duration) -> Result<super::ShmemConnection> {
        Ok(super::ShmemConnection::from_sync(
            self.0.accept_timeout(timeout).await?,
        ))
    }

    pub fn cleanup(name: &str) {
        imp::Listener::cleanup(name);
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    use ::tokio::io::{AsyncReadExt, AsyncWriteExt};
    use ::tokio::net::{UnixListener, UnixStream};

    use crate::connection::ShmemConnection;

    fn handshake_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/tmp/shmem_ipc_{name}.sock"))
    }

    pub struct Listener {
        inner: UnixListener,
        name: String,
        config: ChannelConfig,
        counter: u64,
    }

    impl Listener {
        pub async fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
            let path = handshake_path(name);
            let _ = std::fs::remove_file(&path);
            let inner = UnixListener::bind(&path)?;
            Ok(Self {
                inner,
                name: name.to_string(),
                config,
                counter: 0,
            })
        }

        pub async fn accept(&mut self) -> Result<ShmemConnection> {
            let (stream, _) = self.inner.accept().await?;
            self.finish_accept(stream).await
        }

        pub async fn accept_timeout(&mut self, timeout: Duration) -> Result<ShmemConnection> {
            let stream = match ::tokio::time::timeout(timeout, self.inner.accept()).await {
                Ok(result) => result?.0,
                Err(_) => return Err(Error::TimedOut),
            };
            self.finish_accept(stream).await
        }

        fn cleanup_path(name: &str) -> std::path::PathBuf {
            handshake_path(name)
        }

        async fn finish_accept(&mut self, mut stream: UnixStream) -> Result<ShmemConnection> {
            let conn_name = format!("{}.conn.{}", self.name, self.counter);
            self.counter += 1;

            let channel = crate::Channel::create_with_config(&conn_name, self.config.clone())?;
            send_conn_name(&mut stream, &conn_name).await?;
            Ok(channel.into_connection())
        }

        pub fn cleanup(name: &str) {
            let _ = std::fs::remove_file(Self::cleanup_path(name));
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(Self::cleanup_path(&self.name));
        }
    }

    pub async fn connect_to(name: &str, config: ChannelConfig) -> Result<ShmemConnection> {
        let path = handshake_path(name);
        let deadline = std::time::Instant::now() + config.connect_timeout;

        let mut stream = loop {
            match UnixStream::connect(&path).await {
                Ok(stream) => break stream,
                Err(err)
                    if std::time::Instant::now() < deadline
                        && matches!(
                            err.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                {
                    ::tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(err) => return Err(Error::Io(err)),
            }
        };

        let conn_name = recv_conn_name(&mut stream).await?;
        let channel = crate::Channel::open_with_config(&conn_name, config)?;
        Ok(channel.into_connection())
    }

    async fn send_conn_name(stream: &mut UnixStream, conn_name: &str) -> Result<()> {
        let bytes = conn_name.as_bytes();
        let len = bytes.len() as u16;
        stream.write_all(&len.to_le_bytes()).await?;
        stream.write_all(bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn recv_conn_name(stream: &mut UnixStream) -> Result<String> {
        let mut len_buf = [0u8; 2];
        stream.read_exact(&mut len_buf).await?;
        let len = u16::from_le_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        String::from_utf8(buf)
            .map_err(|err| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err)))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    use ::tokio::io::{AsyncReadExt, AsyncWriteExt};
    use ::tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
    };
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    use crate::connection::ShmemConnection;

    fn pipe_name(name: &str) -> String {
        format!(r"\\.\pipe\shmem_ipc_{name}")
    }

    pub struct Listener {
        current: NamedPipeServer,
        name: String,
        config: ChannelConfig,
        counter: u64,
        pipe_path: String,
    }

    impl Listener {
        pub async fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
            let pipe_path = pipe_name(name);
            let current = create_server(&pipe_path, true)?;
            Ok(Self {
                current,
                name: name.to_string(),
                config,
                counter: 0,
                pipe_path,
            })
        }

        pub async fn accept(&mut self) -> Result<ShmemConnection> {
            self.wait_for_connection(None).await
        }

        pub async fn accept_timeout(&mut self, timeout: Duration) -> Result<ShmemConnection> {
            self.wait_for_connection(Some(timeout)).await
        }

        pub fn cleanup(name: &str) {
            crate::ShmemListener::cleanup(name);
        }

        async fn wait_for_connection(
            &mut self,
            timeout: Option<Duration>,
        ) -> Result<ShmemConnection> {
            match timeout {
                Some(timeout) => {
                    match ::tokio::time::timeout(timeout, self.current.connect()).await {
                        Ok(result) => result?,
                        Err(_) => return Err(Error::TimedOut),
                    }
                }
                None => self.current.connect().await?,
            }

            let connected =
                std::mem::replace(&mut self.current, create_server(&self.pipe_path, false)?);
            self.finish_accept(connected).await
        }

        async fn finish_accept(&mut self, mut pipe: NamedPipeServer) -> Result<ShmemConnection> {
            let conn_name = format!("{}.conn.{}", self.name, self.counter);
            self.counter += 1;

            let channel = crate::Channel::create_with_config(&conn_name, self.config.clone())?;
            send_conn_name(&mut pipe, &conn_name).await?;
            Ok(channel.into_connection())
        }
    }

    pub async fn connect_to(name: &str, config: ChannelConfig) -> Result<ShmemConnection> {
        let pipe_path = pipe_name(name);
        let deadline = std::time::Instant::now() + config.connect_timeout;
        let mut client = loop {
            match create_client(&pipe_path) {
                Ok(client) => break client,
                Err(err)
                    if std::time::Instant::now() < deadline
                        && (err.kind() == std::io::ErrorKind::NotFound
                            || err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)) =>
                {
                    ::tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(err) => return Err(Error::Io(err)),
            }
        };

        let conn_name = recv_conn_name(&mut client).await?;
        let channel = crate::Channel::open_with_config(&conn_name, config)?;
        Ok(channel.into_connection())
    }

    fn create_server(pipe_path: &str, first_instance: bool) -> std::io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options.first_pipe_instance(first_instance);
        options.pipe_mode(PipeMode::Byte);
        options.in_buffer_size(4096);
        options.out_buffer_size(4096);
        options.create(pipe_path)
    }

    fn create_client(pipe_path: &str) -> std::io::Result<NamedPipeClient> {
        let mut options = ClientOptions::new();
        options.pipe_mode(PipeMode::Byte);
        options.open(pipe_path)
    }

    async fn send_conn_name(pipe: &mut NamedPipeServer, conn_name: &str) -> Result<()> {
        let bytes = conn_name.as_bytes();
        let len = bytes.len() as u16;
        pipe.write_all(&len.to_le_bytes()).await?;
        pipe.write_all(bytes).await?;
        pipe.flush().await?;
        Ok(())
    }

    async fn recv_conn_name(pipe: &mut NamedPipeClient) -> Result<String> {
        let mut len_buf = [0u8; 2];
        pipe.read_exact(&mut len_buf).await?;
        let len = u16::from_le_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        pipe.read_exact(&mut buf).await?;
        String::from_utf8(buf)
            .map_err(|err| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err)))
    }
}
