// SPDX-License-Identifier: MIT

use std::{
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::fd::{AsFd, OwnedFd},
};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

use super::super::metadata::MAX_METADATA_BYTES;
use crate::services::ImagePreviewSize;

pub(super) const MAX_OUTPUT_BYTES: u64 = super::super::MAX_OUTPUT_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Image,
    Raw,
    Pdf,
    Video,
    ImageMetadata,
    MediaMetadata,
    PreviewImage(ImagePreviewSize),
    DocumentMermaid,
    DocumentMath,
    DocumentMathInline,
    ThreeMfThumbnail,
    FreeCadThumbnail,
}

const PREVIEW_IMAGE: u8 = 7;
/// The operation byte, then a preview image's box as two little-endian `u32`s.
const MAX_REQUEST_BYTES: usize = 9;

impl Operation {
    fn code(self) -> u8 {
        match self {
            Self::Image => 1,
            Self::Raw => 2,
            Self::Pdf => 3,
            Self::Video => 4,
            Self::ImageMetadata => 5,
            Self::MediaMetadata => 6,
            Self::PreviewImage(_) => PREVIEW_IMAGE,
            Self::DocumentMermaid => 8,
            Self::DocumentMath => 9,
            Self::DocumentMathInline => 10,
            Self::ThreeMfThumbnail => 11,
            Self::FreeCadThumbnail => 12,
        }
    }

    fn encode(self) -> ([u8; MAX_REQUEST_BYTES], usize) {
        let mut bytes = [0; MAX_REQUEST_BYTES];
        bytes[0] = self.code();
        if let Self::PreviewImage(size) = self {
            bytes[1..5].copy_from_slice(&size.width.to_le_bytes());
            bytes[5..9].copy_from_slice(&size.height.to_le_bytes());
            return (bytes, MAX_REQUEST_BYTES);
        }
        (bytes, 1)
    }

    pub(super) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let unknown = || io::Error::other("Unknown browser operation");
        let (&code, payload) = bytes.split_first().ok_or_else(unknown)?;
        if payload.len() + 1 != Self::request_len(code) {
            return Err(unknown());
        }
        Ok(match code {
            1 => Self::Image,
            2 => Self::Raw,
            3 => Self::Pdf,
            4 => Self::Video,
            5 => Self::ImageMetadata,
            6 => Self::MediaMetadata,
            PREVIEW_IMAGE => {
                let edge = |at: usize| {
                    let edge = u32::from_le_bytes(payload[at..at + 4].try_into().expect("fixed"));
                    i32::try_from(edge).unwrap_or(i32::MAX)
                };
                Self::PreviewImage(ImagePreviewSize::new(edge(0), edge(4)))
            }
            8 => Self::DocumentMermaid,
            9 => Self::DocumentMath,
            10 => Self::DocumentMathInline,
            11 => Self::ThreeMfThumbnail,
            12 => Self::FreeCadThumbnail,
            _ => return Err(unknown()),
        })
    }

    fn request_len(code: u8) -> usize {
        if code == PREVIEW_IMAGE {
            MAX_REQUEST_BYTES
        } else {
            1
        }
    }

    pub(crate) fn output_limit(self) -> u64 {
        match self {
            Self::PreviewImage(size) => super::super::preview_image_output_limit(size),
            _ => MAX_OUTPUT_BYTES,
        }
    }
}

pub(super) fn send(
    socket: &impl AsFd,
    input: &impl AsFd,
    output: &impl AsFd,
    operation: Operation,
) -> io::Result<()> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let descriptors = [input.as_fd(), output.as_fd()];
    if !ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)) {
        return Err(io::Error::other("Unable to send browser input"));
    }
    let (bytes, length) = operation.encode();
    let count = sendmsg(
        socket,
        &[io::IoSlice::new(&bytes[..length])],
        &mut ancillary,
        SendFlags::NOSIGNAL,
    )?;
    if count != length {
        return Err(io::Error::other("Incomplete browser request"));
    }
    Ok(())
}

pub(super) fn receive(socket: &impl AsFd) -> io::Result<Option<(Operation, OwnedFd, OwnedFd)>> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let mut bytes = [0; MAX_REQUEST_BYTES];
    let result = recvmsg(
        socket,
        &mut [io::IoSliceMut::new(&mut bytes)],
        &mut ancillary,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let descriptors: Vec<_> = ancillary
        .drain()
        .flat_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => fds.collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    if result.bytes == 0 && descriptors.is_empty() {
        return Ok(None);
    }
    let expected = Operation::request_len(bytes[0]);
    if result.bytes != expected
        || result
            .flags
            .intersects(rustix::net::ReturnFlags::TRUNC | rustix::net::ReturnFlags::CTRUNC)
        || descriptors.len() != 2
    {
        return Err(io::Error::other("Invalid browser request descriptors"));
    }
    let mut descriptors = descriptors.into_iter();
    Ok(Some((
        Operation::parse(&bytes[..expected])?,
        descriptors
            .next()
            .ok_or_else(|| io::Error::other("Missing input"))?,
        descriptors
            .next()
            .ok_or_else(|| io::Error::other("Missing output"))?,
    )))
}

#[derive(Default)]
pub(crate) struct Response {
    pub(crate) png: Vec<u8>,
    pub(crate) metadata: Vec<u8>,
}

impl Response {
    pub(super) fn read(reader: &mut impl Read, png_limit: u64) -> io::Result<Self> {
        let mut header = [0; 8];
        reader.read_exact(&mut header)?;
        let png = u32::from_le_bytes(header[..4].try_into().expect("fixed header")) as usize;
        let metadata = u32::from_le_bytes(header[4..].try_into().expect("fixed header")) as usize;
        if png as u64 > png_limit || metadata as u64 > MAX_METADATA_BYTES {
            return Err(io::Error::other("Oversized browser response"));
        }
        let mut response = Self {
            png: vec![0; png],
            metadata: vec![0; metadata],
        };
        reader.read_exact(&mut response.png)?;
        reader.read_exact(&mut response.metadata)?;
        Ok(response)
    }

    pub(super) fn write(&self, writer: &mut impl Write, png_limit: u64) -> io::Result<()> {
        if self.png.len() as u64 > png_limit || self.metadata.len() as u64 > MAX_METADATA_BYTES {
            return Err(io::Error::other("Oversized browser response"));
        }
        writer.write_all(&(self.png.len() as u32).to_le_bytes())?;
        writer.write_all(&(self.metadata.len() as u32).to_le_bytes())?;
        writer.write_all(&self.png)?;
        writer.write_all(&self.metadata)?;
        writer.flush()
    }
}
