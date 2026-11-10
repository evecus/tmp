// zip.rs — streaming ZIP writer. Mirrors Go archive/zip as driven by the sfh
// handler (zw.Create(rel)): Deflate method emitted as deflate "stored" blocks
// (valid deflate, same uncompressed content), data descriptors (flag 0x8),
// UTF-8 flag (0x800), zero ModTime (no UT extra), CreatorVersion 0x0014,
// zero external attrs, and ZIP64 records for entries/archives over 4 GB.

use std::io::{self, Write};

use crate::util::Crc32;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CD_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOC_SIG: u32 = 0x0706_4b50;
const DESC_SIG: u32 = 0x0807_4b50;

const VERSION_20: u16 = 20;
const VERSION_45: u16 = 45;

pub struct CentralEntry {
    name: String,
    crc: u32,
    usize_: u64,
    csize: u64,
    offset: u64,
    zip64: bool,
}

struct CurrentFile {
    name: String,
    crc: Crc32,
    count: u64,
    entry_offset: u64,
}

pub struct ZipWriter<W: Write> {
    w: W,
    offset: u64,
    entries: Vec<CentralEntry>,
    cur: Option<CurrentFile>,
}

impl<W: Write> ZipWriter<W> {
    pub fn new(w: W) -> Self {
        ZipWriter {
            w,
            offset: 0,
            entries: Vec::new(),
            cur: None,
        }
    }

    /// Begin a new file entry. Mirrors Go Writer.Create: header with Name
    /// only, Method Deflate, zero ModTime → DOS time/date 0, no extra field,
    /// flags 0x8 (+0x800 for non-ASCII names), sizes deferred to the data
    /// descriptor, ReaderVersion 20.
    pub fn start_file(&mut self, name: &str) -> io::Result<()> {
        self.finish_file_if_open()?;
        let mut flags: u16 = 0x8; // data descriptor follows
        if !name.is_ascii() {
            flags |= 0x800; // UTF-8 name
        }
        let name_bytes = name.as_bytes();

        let mut hdr = Vec::with_capacity(30 + name_bytes.len());
        hdr.extend_from_slice(&LOCAL_SIG.to_le_bytes());
        hdr.extend_from_slice(&VERSION_20.to_le_bytes()); // ReaderVersion
        hdr.extend_from_slice(&flags.to_le_bytes());
        hdr.extend_from_slice(&8u16.to_le_bytes()); // method: deflate
        hdr.extend_from_slice(&0u16.to_le_bytes()); // mod time (zero)
        hdr.extend_from_slice(&0u16.to_le_bytes()); // mod date (zero)
        hdr.extend_from_slice(&0u32.to_le_bytes()); // crc (in descriptor)
        hdr.extend_from_slice(&0u32.to_le_bytes()); // csize (in descriptor)
        hdr.extend_from_slice(&0u32.to_le_bytes()); // usize (in descriptor)
        hdr.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        hdr.extend_from_slice(&0u16.to_le_bytes()); // extra len (none)
        hdr.extend_from_slice(name_bytes);

        self.w.write_all(&hdr)?;
        self.offset += hdr.len() as u64;
        self.cur = Some(CurrentFile {
            name: name.to_string(),
            crc: Crc32::new(),
            count: 0,
            entry_offset: self.offset - hdr.len() as u64,
        });
        Ok(())
    }

    /// Write raw file data (compressed on the fly as stored-deflate blocks).
    pub fn write_data(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let cur = self
            .cur
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "zip: no open file"))?;
        cur.crc.update(data);
        cur.count += data.len() as u64;
        // Emit the data as stored blocks of ≤ 65535 bytes.
        let mut off = 0usize;
        while off < data.len() {
            let take = std::cmp::min(65535, data.len() - off);
            self.write_stored_block(&data[off..off + take], false)?;
            off += take;
        }
        Ok(())
    }

    /// Close the current entry (flush final block, write data descriptor).
    pub fn finish_file_if_open(&mut self) -> io::Result<()> {
        let Some(cur) = self.cur.take() else {
            return Ok(());
        };
        // every deflate stream ends with a final (possibly empty) stored block
        self.write_stored_block(&[], true)?;
        let crc = cur.crc.finish();
        let usize_ = cur.count;
        let csize = cur.count + stored_block_overhead(cur.count);
        let zip64 = usize_ > 0xFFFF_FFFF || csize > 0xFFFF_FFFF;
        let mut desc = Vec::with_capacity(24);
        desc.extend_from_slice(&DESC_SIG.to_le_bytes());
        desc.extend_from_slice(&crc.to_le_bytes());
        if zip64 {
            desc.extend_from_slice(&csize.to_le_bytes());
            desc.extend_from_slice(&usize_.to_le_bytes());
        } else {
            desc.extend_from_slice(&(csize as u32).to_le_bytes());
            desc.extend_from_slice(&(usize_ as u32).to_le_bytes());
        }
        self.w.write_all(&desc)?;
        self.offset += desc.len() as u64;

        self.entries.push(CentralEntry {
            name: cur.name,
            crc,
            usize_,
            csize,
            offset: cur.entry_offset,
            zip64,
        });
        Ok(())
    }

    fn write_stored_block(&mut self, data: &[u8], last: bool) -> io::Result<()> {
        let n = data.len() as u16;
        let mut buf = Vec::with_capacity(5 + data.len());
        buf.push(if last { 1 } else { 0 }); // BFINAL | BTYPE=00
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&(!n).to_le_bytes());
        buf.extend_from_slice(data);
        self.w.write_all(&buf)?;
        self.offset += buf.len() as u64;
        Ok(())
    }

    /// Write central directory + end records (Go Writer.Close layout:
    /// CreatorVersion 0x0314, ReaderVersion 20, UT extra then zip64 extra).
    pub fn finish(mut self) -> io::Result<()> {
        self.finish_file_if_open()?;
        let cd_start = self.offset;
        let n_entries = self.entries.len();
        for e in &self.entries {
            let mut extra: Vec<u8> = Vec::new();
            // zip64 extra appended after (Go Close), sizes/offset maxed in record
            let zip64 = e.usize_ >= 0xFFFF_FFFF || e.csize >= 0xFFFF_FFFF;
            let usz = if zip64 { 0xFFFF_FFFFu32 } else { e.usize_ as u32 };
            let csz = if zip64 { 0xFFFF_FFFFu32 } else { e.csize as u32 };
            if zip64 {
                extra.extend_from_slice(&0x0001u16.to_le_bytes());
                extra.extend_from_slice(&24u16.to_le_bytes());
                extra.extend_from_slice(&e.usize_.to_le_bytes());
                extra.extend_from_slice(&e.csize.to_le_bytes());
                extra.extend_from_slice(&e.offset.to_le_bytes());
            }
            let flags: u16 = 0x8 | if !e.name.is_ascii() { 0x800 } else { 0 };
            let mut cd = Vec::with_capacity(46 + e.name.len() + extra.len());
            cd.extend_from_slice(&CD_SIG.to_le_bytes());
            cd.extend_from_slice(&0x0014u16.to_le_bytes()); // CreatorVersion (Go Create default)
            let rv = if e.zip64 { VERSION_45 } else { VERSION_20 };
            cd.extend_from_slice(&rv.to_le_bytes()); // ReaderVersion
            cd.extend_from_slice(&flags.to_le_bytes());
            cd.extend_from_slice(&8u16.to_le_bytes()); // deflate
            cd.extend_from_slice(&0u16.to_le_bytes()); // mod time (zero)
            cd.extend_from_slice(&0u16.to_le_bytes()); // mod date (zero)
            cd.extend_from_slice(&e.crc.to_le_bytes());
            cd.extend_from_slice(&csz.to_le_bytes());
            cd.extend_from_slice(&usz.to_le_bytes());
            cd.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes()); // comment len
            cd.extend_from_slice(&0u16.to_le_bytes()); // disk number start
            cd.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            cd.extend_from_slice(&0u32.to_le_bytes()); // external attrs (zero, Go Create)
            let off = if e.offset >= 0xFFFF_FFFF { 0xFFFF_FFFF } else { e.offset as u32 };
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(e.name.as_bytes());
            cd.extend_from_slice(&extra);
            self.w.write_all(&cd)?;
            self.offset += cd.len() as u64;
        }
        let cd_size = self.offset - cd_start;

        let need_zip64_eocd = n_entries > 0xFFFF
            || cd_start > 0xFFFF_FFFF
            || cd_size > 0xFFFF_FFFF
            || self.entries.iter().any(|e| e.zip64);
        if need_zip64_eocd {
            let z64_off = self.offset;
            let mut z = Vec::with_capacity(56 + 20);
            z.extend_from_slice(&ZIP64_EOCD_SIG.to_le_bytes());
            z.extend_from_slice(&44u64.to_le_bytes());
            z.extend_from_slice(&VERSION_45.to_le_bytes());
            z.extend_from_slice(&VERSION_45.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&(n_entries as u64).to_le_bytes());
            z.extend_from_slice(&(n_entries as u64).to_le_bytes());
            z.extend_from_slice(&cd_size.to_le_bytes());
            z.extend_from_slice(&cd_start.to_le_bytes());
            self.w.write_all(&z)?;
            let mut loc = Vec::with_capacity(20);
            loc.extend_from_slice(&ZIP64_LOC_SIG.to_le_bytes());
            loc.extend_from_slice(&0u32.to_le_bytes());
            loc.extend_from_slice(&z64_off.to_le_bytes());
            loc.extend_from_slice(&1u32.to_le_bytes());
            self.w.write_all(&loc)?;
        }

        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&EOCD_SIG.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        let n16 = if n_entries > 0xFFFF { 0xFFFF } else { n_entries as u16 };
        eocd.extend_from_slice(&n16.to_le_bytes());
        eocd.extend_from_slice(&n16.to_le_bytes());
        eocd.extend_from_slice(
            &(if cd_size > 0xFFFF_FFFF { 0xFFFF_FFFFu32 } else { cd_size as u32 }).to_le_bytes(),
        );
        eocd.extend_from_slice(
            &(if cd_start > 0xFFFF_FFFF { 0xFFFF_FFFFu32 } else { cd_start as u32 }).to_le_bytes(),
        );
        eocd.extend_from_slice(&0u16.to_le_bytes());
        self.w.write_all(&eocd)?;
        self.w.flush()
    }
}

fn stored_block_overhead(count: u64) -> u64 {
    let mut blocks = count / 65535;
    if count % 65535 != 0 {
        blocks += 1;
    }
    (blocks + 1) * 5 // + final empty block
}
