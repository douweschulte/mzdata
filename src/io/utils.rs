#![allow(dead_code)]

use std::io;

use std::fs;
use std::path;

type ByteBuffer = io::Cursor<Vec<u8>>;

#[derive(Debug, Clone)]
pub enum FileWrapper<T: io::Read> {
    FileSystem(path::PathBuf),
    Stream(T),
    Empty,
}

impl<T: io::Read> Default for FileWrapper<T> {
    fn default() -> FileWrapper<T> {
        FileWrapper::Empty
    }
}

#[derive(Debug, Clone, Default)]
pub struct FileSource<T: io::Read> {
    pub source: FileWrapper<T>,
}

/// This really should be a full file-like object abstraction, but that
/// feels like it is beyond the scope of this crate. Something like
/// https://github.com/bnjjj/chicon-rs
impl<'lifespan, T: io::Read> FileSource<T> {
    pub fn from_path<P>(path: P) -> FileSource<T>
    where
        P: Into<path::PathBuf>,
    {
        FileSource {
            source: FileWrapper::FileSystem(path.into()),
        }
    }

    pub fn from_stream(stream: T) -> FileSource<T> {
        FileSource {
            source: FileWrapper::Stream(stream),
        }
    }

    pub fn file_name(&self) -> Option<&path::Path> {
        match &self.source {
            FileWrapper::FileSystem(path) => Some(path),
            FileWrapper::Stream(_stream) => None,
            FileWrapper::Empty => None,
        }
    }

    pub fn index_file_name(&self) -> Option<path::PathBuf> {
        match &self.source {
            FileWrapper::Empty => None,
            FileWrapper::Stream(_stream) => None,
            FileWrapper::FileSystem(path) => {
                if let Some(stem) = path.file_name() {
                    if let Some(parent) = path.parent() {
                        let base = parent.join(stem);
                        let name = base.with_extension("index.json");
                        return Some(name);
                    }
                }
                None
            }
        }
    }

    pub fn has_index_file(&self) -> bool {
        match self.index_file_name() {
            Some(path) => path.exists(),
            None => false,
        }
    }
}

pub fn from_path<P>(path: P) -> FileSource<fs::File>
where
    P: Into<path::PathBuf>,
{
    FileSource::from_path(path)
}

impl<T, P> From<P> for FileSource<T>
where
    P: Into<path::PathBuf>,
    T: io::Read,
{
    fn from(path: P) -> FileSource<T> {
        FileSource::from_path(path)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::prelude::*;

    #[test]
    fn test_from_buffer() {
        let mut buff: Vec<u8> = Vec::new();
        buff.extend(b"foobar");
        let stream = ByteBuffer::new(buff);
        let mut out: Vec<u8> = Vec::new();
        let desc = FileSource::<ByteBuffer>::from_stream(stream);
        assert!(matches!(desc.file_name(), None));
        if let FileWrapper::Stream(mut buff) = desc.source {
            buff.read_to_end(&mut out).unwrap();
            assert_eq!(out, b"foobar");
        }
    }
}