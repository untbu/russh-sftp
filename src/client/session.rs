use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{
    error::Error,
    fs::{File, Metadata, ReadDir},
    rawsession::{Limits, SftpResult},
    RawSftpSession,
};
use crate::{
    extensions::{self, Statvfs},
    protocol::{FileAttributes, OpenFlags, StatusCode},
};

#[derive(Debug, Default)]
pub(crate) struct Extensions {
    pub hardlink: bool,
    pub fsync: bool,
    pub statvfs: bool,
    pub limits: Option<Arc<Limits>>,
}

/// High-level SFTP implementation for easy interaction with a remote file system.
/// Contains most methods similar to the native [filesystem](std::fs)
pub struct SftpSession {
    session: Arc<RawSftpSession>,
    extensions: Arc<Extensions>,
}

impl SftpSession {
    /// Creates a new session by initializing the protocol and extensions
    pub async fn new<S>(stream: S) -> SftpResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::new_opts(stream, None).await
    }

    /// Creates a new session with timeout opt before the first request
    pub async fn new_opts<S>(stream: S, timeout: Option<u64>) -> SftpResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut session = RawSftpSession::new(stream);

        // todo: for new options we need builder
        if let Some(timeout) = timeout {
            session.set_timeout(timeout).await;
        }

        let version = session.init().await?;
        let mut extensions = Extensions {
            hardlink: version
                .extensions
                .get(extensions::HARDLINK)
                .is_some_and(|e| e == "1"),
            fsync: version
                .extensions
                .get(extensions::FSYNC)
                .is_some_and(|e| e == "1"),
            statvfs: version
                .extensions
                .get(extensions::STATVFS)
                .is_some_and(|e| e == "2"),
            limits: None,
        };

        if version
            .extensions
            .get(extensions::LIMITS)
            .is_some_and(|e| e == "1")
        {
            let limits = session.limits().await?;
            let limits = Arc::new(Limits::from(limits));

            session.set_limits(limits.clone());
            extensions.limits = Some(limits);
        }

        Ok(Self {
            session: Arc::new(session),
            extensions: Arc::new(extensions),
        })
    }

    /// Set the maximum response time in seconds.
    /// Default: 10 seconds
    pub async fn set_timeout(&self, secs: u64) {
        self.session.set_timeout(secs).await;
    }

    /// Closes the inner channel stream.
    pub async fn close(&self) -> SftpResult<()> {
        self.session.close_session()
    }

    /// Attempts to open a file in read-only mode.
    pub async fn open<T: Into<String>>(&self, filename: T) -> SftpResult<File> {
        self.open_with_flags(filename, OpenFlags::READ).await
    }

    /// Opens a file in write-only mode.
    ///
    /// This function will create a file if it does not exist, and will truncate it if it does.
    pub async fn create<T: Into<String>>(&self, filename: T) -> SftpResult<File> {
        self.open_with_flags(
            filename,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
    }

    /// Attempts to open or create the file in the specified mode
    pub async fn open_with_flags<T: Into<String>>(
        &self,
        filename: T,
        flags: OpenFlags,
    ) -> SftpResult<File> {
        self.open_with_flags_and_attributes(filename, flags, FileAttributes::empty())
            .await
    }

    /// Attempts to open or create the file in the specified mode and with specified file attributes
    pub async fn open_with_flags_and_attributes<T: Into<String>>(
        &self,
        filename: T,
        flags: OpenFlags,
        attributes: FileAttributes,
    ) -> SftpResult<File> {
        let handle = self.session.open(filename, flags, attributes).await?.handle;
        Ok(File::new(
            self.session.clone(),
            handle,
            self.extensions.clone(),
        ))
    }

    /// Requests the remote party for the absolute from the relative path.
    pub async fn canonicalize<T: Into<String>>(&self, path: T) -> SftpResult<String> {
        let name = self.session.realpath(path).await?;
        match name.files.first() {
            Some(file) => Ok(file.filename.to_owned()),
            None => Err(Error::UnexpectedBehavior("no file".to_owned())),
        }
    }

    /// Creates a new empty directory.
    pub async fn create_dir<T: Into<String>>(&self, path: T) -> SftpResult<()> {
        self.session
            .mkdir(path, FileAttributes::empty())
            .await
            .map(|_| ())
    }

    /// Reads the contents of a file located at the specified path to the end.
    pub async fn read<P: Into<String>>(&self, path: P) -> SftpResult<Vec<u8>> {
        let mut file = self.open(path).await?;
        let mut buffer = Vec::new();

        file.read_to_end(&mut buffer).await?;

        Ok(buffer)
    }

    /// Writes the contents to a file whose path is specified.
    pub async fn write<P: Into<String>>(&self, path: P, data: &[u8]) -> SftpResult<()> {
        let mut file = self.open_with_flags(path, OpenFlags::WRITE).await?;
        file.write_all(data).await?;
        Ok(())
    }

    /// Checks a file or folder exists at the specified path
    pub async fn try_exists<P: Into<String>>(&self, path: P) -> SftpResult<bool> {
        match self.metadata(path).await {
            Ok(_) => Ok(true),
            Err(Error::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Returns an iterator over the entries within a directory.
    pub async fn read_dir<P: Into<String>>(&self, path: P) -> SftpResult<ReadDir> {
        let mut files = vec![];
        let handle = self.session.opendir(path).await?.handle;

        loop {
            match self.session.readdir(handle.as_str()).await {
                Ok(name) => {
                    files = name
                        .files
                        .into_iter()
                        .map(|f| (f.filename, f.attrs))
                        .chain(files.into_iter())
                        .collect();
                }
                Err(Error::Status(status)) if status.status_code == StatusCode::Eof => break,
                Err(err) => return Err(err),
            }
        }

        self.session.close(handle).await?;

        Ok(ReadDir {
            entries: files.into(),
        })
    }

    /// Reads a symbolic link, returning the file that the link points to.
    pub async fn read_link<P: Into<String>>(&self, path: P) -> SftpResult<String> {
        let name = self.session.readlink(path).await?;
        match name.files.first() {
            Some(file) => Ok(file.filename.to_owned()),
            None => Err(Error::UnexpectedBehavior("no file".to_owned())),
        }
    }

    /// Removes the specified folder.
    pub async fn remove_dir<P: Into<String>>(&self, path: P) -> SftpResult<()> {
        self.session.rmdir(path).await.map(|_| ())
    }

    /// Removes the specified file.
    pub async fn remove_file<T: Into<String>>(&self, filename: T) -> SftpResult<()> {
        self.session.remove(filename).await.map(|_| ())
    }

    /// Rename a file or directory to a new name.
    pub async fn rename<O, N>(&self, oldpath: O, newpath: N) -> SftpResult<()>
    where
        O: Into<String>,
        N: Into<String>,
    {
        self.session.rename(oldpath, newpath).await.map(|_| ())
    }

    /// Creates a symlink of the specified target.
    pub async fn symlink<P, T>(&self, path: P, target: T) -> SftpResult<()>
    where
        P: Into<String>,
        T: Into<String>,
    {
        self.session.symlink(path, target).await.map(|_| ())
    }

    /// Queries metadata about the remote file.
    pub async fn metadata<P: Into<String>>(&self, path: P) -> SftpResult<Metadata> {
        Ok(self.session.stat(path).await?.attrs)
    }

    /// Sets metadata for a remote file.
    pub async fn set_metadata<P: Into<String>>(
        &self,
        path: P,
        metadata: Metadata,
    ) -> Result<(), Error> {
        self.session.setstat(path, metadata).await.map(|_| ())
    }

    pub async fn symlink_metadata<P: Into<String>>(&self, path: P) -> SftpResult<Metadata> {
        Ok(self.session.lstat(path).await?.attrs)
    }

    pub async fn hardlink<O, N>(&self, oldpath: O, newpath: N) -> SftpResult<bool>
    where
        O: Into<String>,
        N: Into<String>,
    {
        if !self.extensions.hardlink {
            return Ok(false);
        }

        self.session.hardlink(oldpath, newpath).await.map(|_| true)
    }

    /// Performs a statvfs on the remote file system path.
    /// Returns [`Ok(None)`] if the remote SFTP server does not support `statvfs@openssh.com` extension v2.
    pub async fn fs_info<P: Into<String>>(&self, path: P) -> SftpResult<Option<Statvfs>> {
        if !self.extensions.statvfs {
            return Ok(None);
        }

        self.session.statvfs(path).await.map(Some)
    }
}
