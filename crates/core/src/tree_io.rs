//! Compact binary encoding of a [`ScanResult`], used between the privileged scan
//! helper (writes) and the GUI (reads). The reader treats its input as untrusted:
//! sizes are bounded, depth is capped and a missing end marker means "truncated".

use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use crate::scan::{Node, NodeKind, ScanResult};

const MAGIC: &[u8; 6] = b"ORBT1\n";
const END: &[u8; 4] = b"END!";
const MAX_DEPTH: usize = 4096;
const MAX_NODES: u64 = 100_000_000;
const MAX_NAME: usize = 4096;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

pub fn write_result<W: Write>(w: &mut W, r: &ScanResult) -> io::Result<()> {
    w.write_all(MAGIC)?;
    write_node(w, &r.root)?;
    w.write_all(&r.errors.to_le_bytes())?;
    w.write_all(&(r.unreadable.len() as u32).to_le_bytes())?;
    for (p, errno) in &r.unreadable {
        let b = p.as_os_str().as_encoded_bytes();
        w.write_all(&(b.len().min(MAX_NAME) as u16).to_le_bytes())?;
        w.write_all(&b[..b.len().min(MAX_NAME)])?;
        w.write_all(&errno.to_le_bytes())?;
    }
    w.write_all(END)?;
    w.flush()
}

fn write_node<W: Write>(w: &mut W, n: &Node) -> io::Result<()> {
    let name = n.name.as_bytes();
    let name = &name[..name.len().min(MAX_NAME)];
    w.write_all(&(name.len() as u16).to_le_bytes())?;
    w.write_all(name)?;
    w.write_all(&[matches!(n.kind, NodeKind::Dir) as u8])?;
    w.write_all(&n.size.to_le_bytes())?;
    w.write_all(&n.file_count.to_le_bytes())?;
    w.write_all(&(n.children.len() as u32).to_le_bytes())?;
    n.children.iter().try_for_each(|c| write_node(w, c))
}

pub fn read_result<R: Read>(r: &mut R) -> io::Result<ScanResult> {
    let mut magic = [0u8; 6];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("not an Orbit scan stream"));
    }
    let mut budget = MAX_NODES;
    let root = read_node(r, 0, &mut budget)?;
    let errors = read_u64(r)?;
    let count = read_u32(r)?.min(1000);
    let mut unreadable = Vec::new();
    for _ in 0..count {
        let len = read_u16(r)? as usize;
        let mut b = vec![0u8; len.min(MAX_NAME)];
        r.read_exact(&mut b)?;
        let errno = read_u32(r)? as i32;
        unreadable.push((PathBuf::from(std::ffi::OsString::from_vec(b)), errno));
    }
    let mut end = [0u8; 4];
    r.read_exact(&mut end)?;
    if &end != END {
        return Err(bad("truncated scan stream"));
    }
    Ok(ScanResult { root, errors, unreadable })
}

fn read_node<R: Read>(r: &mut R, depth: usize, budget: &mut u64) -> io::Result<Node> {
    if depth > MAX_DEPTH || *budget == 0 {
        return Err(bad("scan tree too large"));
    }
    *budget -= 1;
    let len = read_u16(r)? as usize;
    if len > MAX_NAME {
        return Err(bad("name too long"));
    }
    let mut name = vec![0u8; len];
    r.read_exact(&mut name)?;
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let size = read_u64(r)?;
    let file_count = read_u64(r)?;
    let n = read_u32(r)? as u64;
    if n > *budget {
        return Err(bad("scan tree too large"));
    }
    let mut children = Vec::with_capacity(n.min(4096) as usize);
    for _ in 0..n {
        children.push(read_node(r, depth + 1, budget)?);
    }
    Ok(Node {
        name: String::from_utf8_lossy(&name).into(),
        kind: if kind[0] == 1 { NodeKind::Dir } else { NodeKind::File },
        size,
        file_count,
        children,
    })
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ScanResult {
        let leaf = |n: &str, s| Node { name: n.into(), kind: NodeKind::File, size: s, file_count: 1, children: vec![] };
        ScanResult {
            root: Node {
                name: "/".into(),
                kind: NodeKind::Dir,
                size: 300,
                file_count: 5,
                children: vec![
                    Node { name: "usr".into(), kind: NodeKind::Dir, size: 200, file_count: 3, children: vec![leaf("big", 150)] },
                    leaf("swap.img", 100),
                ],
            },
            errors: 2,
            unreadable: vec![(PathBuf::from("/root"), 13), (PathBuf::from("/lost+found"), 13)],
        }
    }

    #[test]
    fn round_trip() {
        let r = sample();
        let mut buf = Vec::new();
        write_result(&mut buf, &r).unwrap();
        let back = read_result(&mut buf.as_slice()).unwrap();
        assert_eq!(back.root.size, 300);
        assert_eq!(back.root.children[0].children[0].name.as_ref(), "big");
        assert_eq!(back.errors, 2);
        assert_eq!(back.unreadable, r.unreadable);
    }

    #[test]
    fn truncated_and_garbage_are_rejected() {
        let mut buf = Vec::new();
        write_result(&mut buf, &sample()).unwrap();
        for cut in [0, 5, 20, buf.len() - 1] {
            assert!(read_result(&mut &buf[..cut]).is_err(), "cut at {cut}");
        }
        assert!(read_result(&mut &b"garbage garbage garbage"[..]).is_err());
        // A claimed child count larger than the budget must not allocate or loop.
        let mut evil = MAGIC.to_vec();
        evil.extend([0, 0, 1]);
        evil.extend(0u64.to_le_bytes());
        evil.extend(0u64.to_le_bytes());
        evil.extend(u32::MAX.to_le_bytes());
        assert!(read_result(&mut evil.as_slice()).is_err());
    }
}
