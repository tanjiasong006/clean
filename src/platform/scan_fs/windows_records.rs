//! Pure parsing for `FILE_ID_EXTD_DIR_INFO` batches. Kept free of Windows
//! types so the record layout and classification can be tested on any host.

use std::io;

use crate::models::EntryKind;

pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_DEVICE: u32 = 0x40;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Offset of the variable-length `FileName` field on 64-bit Windows.
pub const RECORD_HEADER_SIZE: usize = 88;

const NEXT_ENTRY_OFFSET: usize = 0;
const ALLOCATION_SIZE: usize = 48;
const ATTRIBUTES: usize = 56;
const FILE_NAME_LENGTH: usize = 60;
const REPARSE_POINT_TAG: usize = 68;
const FILE_ID: usize = 72;

pub struct Record {
    /// Byte offset to the next record, or zero for the last record in a batch.
    pub next_offset: usize,
    pub name_units: Vec<u16>,
    pub kind: EntryKind,
    pub allocation: i64,
    pub file_id: u128,
    pub is_dot: bool,
}

pub fn parse(buffer: &[u8], start: usize) -> io::Result<Record> {
    let text_start = start
        .checked_add(RECORD_HEADER_SIZE)
        .filter(|&end| end <= buffer.len())
        .ok_or_else(|| malformed("目录条目缓冲区被截断。"))?;
    let next_offset = u32_at(buffer, start + NEXT_ENTRY_OFFSET)? as usize;
    let allocation = i64_at(buffer, start + ALLOCATION_SIZE)?;
    let attributes = u32_at(buffer, start + ATTRIBUTES)?;
    let name_length = u32_at(buffer, start + FILE_NAME_LENGTH)? as usize;
    let reparse_tag = u32_at(buffer, start + REPARSE_POINT_TAG)?;
    let file_id = u128::from_ne_bytes(
        buffer[start + FILE_ID..start + FILE_ID + 16]
            .try_into()
            .map_err(|_| malformed("目录条目缓冲区被截断。"))?,
    );

    if !name_length.is_multiple_of(2) {
        return Err(malformed("目录条目名称长度不是偶数。"));
    }
    let text_end = text_start
        .checked_add(name_length)
        .filter(|&end| end <= buffer.len())
        .ok_or_else(|| malformed("目录条目名称超出缓冲区。"))?;
    if next_offset != 0 && (next_offset < RECORD_HEADER_SIZE || start + next_offset > buffer.len())
    {
        return Err(malformed("目录条目偏移超出缓冲区。"));
    }

    let name_units = buffer[text_start..text_end]
        .chunks_exact(2)
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let is_dot = name_units == [b'.' as u16] || name_units == [b'.' as u16, b'.' as u16];
    Ok(Record {
        next_offset,
        name_units,
        kind: classify(attributes, reparse_tag),
        allocation,
        file_id,
        is_dot,
    })
}

pub fn classify(attributes: u32, reparse_tag: u32) -> EntryKind {
    let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let reparse = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if directory {
        if !reparse {
            EntryKind::Directory
        } else if matches!(
            reparse_tag,
            IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT
        ) {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        }
    } else if reparse {
        if reparse_tag == IO_REPARSE_TAG_SYMLINK {
            EntryKind::Symlink
        } else {
            // Cloud placeholders and other file reparse points stay regular
            // files so their reported allocation is counted without hydrating.
            EntryKind::File
        }
    } else if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
        EntryKind::Other
    } else {
        EntryKind::File
    }
}

fn malformed(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn u32_at(buffer: &[u8], at: usize) -> io::Result<u32> {
    let bytes = buffer
        .get(at..at + 4)
        .ok_or_else(|| malformed("目录条目缓冲区被截断。"))?;
    Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
}

fn i64_at(buffer: &[u8], at: usize) -> io::Result<i64> {
    let bytes = buffer
        .get(at..at + 8)
        .ok_or_else(|| malformed("目录条目缓冲区被截断。"))?;
    Ok(i64::from_ne_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &[u16], attributes: u32, tag: u32, allocation: i64, next: usize) -> Vec<u8> {
        let name_bytes = name.len() * 2;
        let mut buffer = vec![0_u8; RECORD_HEADER_SIZE + name_bytes];
        buffer[NEXT_ENTRY_OFFSET..NEXT_ENTRY_OFFSET + 4]
            .copy_from_slice(&(next as u32).to_ne_bytes());
        buffer[ALLOCATION_SIZE..ALLOCATION_SIZE + 8].copy_from_slice(&allocation.to_ne_bytes());
        buffer[ATTRIBUTES..ATTRIBUTES + 4].copy_from_slice(&attributes.to_ne_bytes());
        buffer[FILE_NAME_LENGTH..FILE_NAME_LENGTH + 4]
            .copy_from_slice(&(name_bytes as u32).to_ne_bytes());
        buffer[REPARSE_POINT_TAG..REPARSE_POINT_TAG + 4].copy_from_slice(&tag.to_ne_bytes());
        for (index, unit) in name.iter().enumerate() {
            let at = RECORD_HEADER_SIZE + index * 2;
            buffer[at..at + 2].copy_from_slice(&unit.to_ne_bytes());
        }
        buffer
    }

    #[test]
    fn parses_names_allocation_and_identity() {
        let name: Vec<u16> = "报告 😀".encode_utf16().collect();
        let mut buffer = record(&name, 0, 0, 4096, 0);
        buffer[FILE_ID..FILE_ID + 16].copy_from_slice(&0x1234_5678_u128.to_ne_bytes());
        let parsed = parse(&buffer, 0).unwrap();
        assert_eq!(parsed.name_units, name);
        assert_eq!(parsed.kind, EntryKind::File);
        assert_eq!(parsed.allocation, 4096);
        assert_eq!(parsed.file_id, 0x1234_5678);
        assert_eq!(parsed.next_offset, 0);
        assert!(!parsed.is_dot);
    }

    #[test]
    fn skips_dot_and_dotdot() {
        for name in [vec![b'.' as u16], vec![b'.' as u16, b'.' as u16]] {
            let buffer = record(&name, FILE_ATTRIBUTE_DIRECTORY, 0, 0, 0);
            assert!(parse(&buffer, 0).unwrap().is_dot);
        }
        let name = vec![b'.' as u16, b'a' as u16];
        let buffer = record(&name, FILE_ATTRIBUTE_DIRECTORY, 0, 0, 0);
        assert!(!parse(&buffer, 0).unwrap().is_dot);
    }

    #[test]
    fn classifies_reparse_points_without_ever_traversing_them() {
        let dir = FILE_ATTRIBUTE_DIRECTORY;
        let link = FILE_ATTRIBUTE_REPARSE_POINT;
        assert_eq!(classify(dir, 0), EntryKind::Directory);
        assert_eq!(
            classify(dir | link, IO_REPARSE_TAG_SYMLINK),
            EntryKind::Symlink
        );
        assert_eq!(
            classify(dir | link, IO_REPARSE_TAG_MOUNT_POINT),
            EntryKind::Symlink
        );
        assert_eq!(classify(dir | link, 0x9000_001A), EntryKind::Other);
        assert_eq!(classify(link, IO_REPARSE_TAG_SYMLINK), EntryKind::Symlink);
        assert_eq!(classify(link, 0x9000_001A), EntryKind::File);
        assert_eq!(classify(0, 0), EntryKind::File);
        assert_eq!(classify(FILE_ATTRIBUTE_DEVICE, 0), EntryKind::Other);
    }

    #[test]
    fn rejects_truncated_and_misaligned_records() {
        let buffer = record(&[0x61], 0, 0, 0, 0);
        assert!(parse(&buffer, 0).is_ok());
        for length in [0, 1, RECORD_HEADER_SIZE - 1, RECORD_HEADER_SIZE + 1] {
            assert!(parse(&buffer[..length.min(buffer.len())], 0).is_err());
        }

        let mut odd = record(&[0x61], 0, 0, 0, 0);
        odd[FILE_NAME_LENGTH..FILE_NAME_LENGTH + 4].copy_from_slice(&1_u32.to_ne_bytes());
        assert!(parse(&odd, 0).is_err());

        let mut long_name = record(&[0x61], 0, 0, 0, 0);
        long_name[FILE_NAME_LENGTH..FILE_NAME_LENGTH + 4].copy_from_slice(&64_u32.to_ne_bytes());
        assert!(parse(&long_name, 0).is_err());

        let mut bad_next = record(&[0x61], 0, 0, 0, 4);
        bad_next[NEXT_ENTRY_OFFSET..NEXT_ENTRY_OFFSET + 4].copy_from_slice(&4_u32.to_ne_bytes());
        assert!(parse(&bad_next, 0).is_err());
    }

    #[test]
    fn chained_records_start_at_the_advertised_offset() {
        let first = record(&[0x61], 0, 0, 100, 92);
        let second = record(&[0x62], 0, 0, 200, 0);
        let mut buffer = first;
        buffer.resize(92, 0);
        buffer.extend_from_slice(&second);
        let one = parse(&buffer, 0).unwrap();
        assert_eq!(one.next_offset, 92);
        assert_eq!(one.allocation, 100);
        let two = parse(&buffer, one.next_offset).unwrap();
        assert_eq!(two.name_units, [0x62]);
        assert_eq!(two.allocation, 200);
        assert_eq!(two.next_offset, 0);
    }

    #[test]
    fn preserves_unpaired_surrogates_in_names() {
        let name = vec![0x61, 0xD800, 0x62];
        let buffer = record(&name, 0, 0, 0, 0);
        assert_eq!(parse(&buffer, 0).unwrap().name_units, name);
    }
}
