#[cfg(test)]
#[macro_use]
extern crate assert_matches;

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use walkdir::WalkDir;

use format::{
    BlobRef, BlobRefKind, DirEnt, FileChunk, FileChunkList, Ino, Inode, InodeAdditional, Rootfs,
};
use oci::Image;

mod fastcdc_fs;
use fastcdc_fs::{ChunkWithData, FastCDCWrapper};

#[derive(Debug)]
pub struct Error {
    msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<serde_cbor::Error> for Error {
    fn from(e: serde_cbor::Error) -> Self {
        Self { msg: e.to_string() }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self { msg: e.to_string() }
    }
}

impl From<walkdir::Error> for Error {
    fn from(e: walkdir::Error) -> Self {
        Self { msg: e.to_string() }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn walker(rootfs: &Path) -> WalkDir {
    // breadth first search for sharing, don't cross filesystems just to be safe, order by file
    // name.
    WalkDir::new(rootfs)
        .contents_first(false)
        .follow_links(false)
        .same_file_system(true)
        .sort_by(|a, b| a.file_name().cmp(b.file_name()))
}

// a struct to hold a directory's information before it can be rendered into a InodeSpecific::Dir
// (aka the offset is unknown because we haven't accumulated all the inodes yet)
struct Dir {
    ino: u64,
    dir_list: Vec<DirEnt>,
    md: fs::Metadata,
    additional: Option<InodeAdditional>,
}

impl Dir {
    fn add_entry(&mut self, p: &Path, ino: Ino) -> io::Result<()> {
        let name = p.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, format!("no path for {}", p.display()))
        })?;
        self.dir_list.push(DirEnt {
            name: name.to_os_string(),
            ino,
        });
        Ok(())
    }
}

// similar to the above, but holding file metadata
struct File {
    ino: u64,
    chunk_list: FileChunkList,
    md: fs::Metadata,
    additional: Option<InodeAdditional>,
}

fn write_chunks_to_oci(oci: &Image, fcdc: &mut FastCDCWrapper) -> io::Result<Vec<FileChunk>> {
    let mut pending_chunks = Vec::<ChunkWithData>::new();
    fcdc.get_pending_chunks(&mut pending_chunks);
    pending_chunks
        .iter_mut()
        .map(|c| {
            let desc = oci.put_blob(&*c.data)?;
            Ok(FileChunk {
                blob: BlobRef {
                    kind: BlobRefKind::Other {
                        digest: desc.digest,
                    },
                    offset: 0,
                },
                len: desc.len,
            })
        })
        .collect::<io::Result<Vec<FileChunk>>>()
}

// merge the first chunk with the previous files and return a BlobRef that references the rest of
// the file
fn merge_chunk_and_prev_files(
    first_chunk: &FileChunk,
    files: &mut Vec<File>,
    prev_files: &mut Vec<File>,
) -> io::Result<BlobRef> {
    let mut used = 0;
    let first_digest = if let BlobRef {
        kind: BlobRefKind::Other { digest },
        ..
    } = first_chunk.blob
    {
        digest
    } else {
        return Err(io::Error::new(io::ErrorKind::Other, "bad blob type"));
    };

    // drain the list of previous files whose content is at the beginning of this chunk
    files.extend(prev_files.drain(..).map(|mut p| {
        let blob = BlobRef {
            offset: used,
            kind: BlobRefKind::Other {
                digest: first_digest,
            },
        };
        let len = p.md.len();
        used += len;
        p.chunk_list.chunks.push(FileChunk { blob, len });
        p
    }));

    // now fix up the first chunk to have the right offset for this file
    Ok(BlobRef {
        kind: BlobRefKind::Other {
            digest: first_digest,
        },
        offset: used,
    })
}

fn inode_encoded_size(num_inodes: usize) -> usize {
    format::cbor_size_of_list_header(num_inodes) + num_inodes * format::INODE_WIRE_SIZE
}

pub fn build_initial_rootfs(rootfs: &Path, oci: &Image) -> Result<Rootfs> {
    let mut dirs = HashMap::<u64, Dir>::new();
    let mut files = Vec::<File>::new();
    let mut pfs_inodes = Vec::<Inode>::new();

    // host to puzzlefs inode mapping for hard link deteciton
    let mut host_to_pfs = HashMap::<u64, Ino>::new();

    let mut cur_ino: u64 = 1;

    let mut fcdc = FastCDCWrapper::new();
    let mut prev_files = Vec::<File>::new();

    for entry in walker(rootfs) {
        let e = entry?;
        let md = e.metadata()?;

        // now that we know the ino of this thing, let's put it in the parent directory (assuming
        // this is not "/" for our image, aka inode #1)
        if cur_ino != 1 {
            // is this a hard link? if so, just use the existing ino we have rendered. otherewise,
            // use a new one
            let the_ino = host_to_pfs.get(&md.ino()).copied().unwrap_or(cur_ino);
            let parent_path = e.path().parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("no parent for {}", e.path().display()),
                )
            })?;
            let parent = dirs
                .get_mut(&fs::symlink_metadata(parent_path)?.ino())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        format!("no pfs inode for {}", e.path().display()),
                    )
                })?;
            parent.add_entry(e.path(), the_ino)?;

            // if it was a hard link, we don't need to actually render it again
            if host_to_pfs.get(&md.ino()).is_some() {
                continue;
            }
        }

        host_to_pfs.insert(md.ino(), cur_ino);

        // render as much of the inode as we can
        let additional = InodeAdditional::new(e.path(), &md)?;
        if md.is_dir() {
            dirs.insert(
                md.ino(),
                Dir {
                    ino: cur_ino,
                    md,
                    dir_list: Vec::<DirEnt>::new(),
                    additional,
                },
            );
        } else if md.is_file() {
            let mut f = fs::File::open(e.path())?;
            io::copy(&mut f, &mut fcdc)?;

            let mut written_chunks = write_chunks_to_oci(&oci, &mut fcdc)?;
            let mut file = File {
                ino: cur_ino,
                md,
                chunk_list: FileChunkList {
                    chunks: Vec::<FileChunk>::new(),
                },
                additional,
            };

            if written_chunks.is_empty() {
                // this file wasn't big enough to cause a chunk to be generated, add it to the list
                // of files pending for this chunk
                prev_files.push(file);
            } else {
                let first_chunk = written_chunks.first().unwrap();
                let fixed_blob =
                    merge_chunk_and_prev_files(first_chunk, &mut files, &mut prev_files)?;
                file.chunk_list.chunks.push(FileChunk {
                    len: first_chunk.len - fixed_blob.offset,
                    blob: fixed_blob,
                });
                file.chunk_list
                    .chunks
                    .append(&mut written_chunks.split_off(1));
            }
        } else {
            let inode = Inode::new_other(cur_ino, &md, None /* TODO: additional */)?;
            pfs_inodes.push(inode);
        }

        cur_ino += 1;
    }

    // all inodes done, we need to finish up the cdc chunking
    fcdc.finish();
    let written_chunks = write_chunks_to_oci(&oci, &mut fcdc)?;
    let leftover: u64 = written_chunks.iter().map(|c| c.len).sum();

    // if we have chunks, we should have files too
    assert!(written_chunks.is_empty() || !prev_files.is_empty());
    assert!(!written_chunks.is_empty() || prev_files.is_empty());

    if !written_chunks.is_empty() {
        let first_chunk = written_chunks.first().unwrap();
        let fixed_blob = merge_chunk_and_prev_files(first_chunk, &mut files, &mut prev_files)?;
        assert!(leftover == fixed_blob.offset);
    }

    // total inode serailized size
    let num_inodes = pfs_inodes.len() + dirs.len() + files.len();
    let inodes_serial_size = inode_encoded_size(num_inodes);

    // TODO: not render this whole thing in memory, stick it all in the same blob, etc.
    let mut dir_buf = Vec::<u8>::new();

    // need the dirs in inode order
    let mut ordered_dirs = {
        let mut v = dirs.values().collect::<Vec<_>>();
        v.sort_by(|a, b| a.ino.cmp(&b.ino));
        v
    };

    // render dirs
    pfs_inodes.extend(
        ordered_dirs
            .drain(..)
            .map(|d| {
                let dirent_offset = inodes_serial_size + dir_buf.len();
                serde_cbor::to_writer(&mut dir_buf, &d.dir_list).map_err(Error::from)?;
                let additional_ref = d
                    .additional
                    .as_ref()
                    .map::<Result<BlobRef>, _>(|add| {
                        let offset = inodes_serial_size + dir_buf.len();
                        serde_cbor::to_writer(&mut dir_buf, &add).map_err(Error::from)?;
                        Ok(BlobRef {
                            offset: offset as u64,
                            kind: BlobRefKind::Local,
                        })
                    })
                    .transpose()?;
                Ok(Inode::new_dir(
                    d.ino,
                    &d.md,
                    dirent_offset as u64,
                    additional_ref,
                )?)
            })
            .collect::<Result<Vec<Inode>>>()?,
    );

    let mut files_buf = Vec::<u8>::new();

    // render files
    pfs_inodes.extend(
        files
            .drain(..)
            .map(|f| {
                let chunk_offset = inodes_serial_size + dir_buf.len() + files_buf.len();
                serde_cbor::to_writer(&mut files_buf, &f.chunk_list).map_err(Error::from)?;
                let additional_ref = f
                    .additional
                    .as_ref()
                    .map::<Result<BlobRef>, _>(|add| {
                        let offset = inodes_serial_size + dir_buf.len() + files_buf.len();
                        serde_cbor::to_writer(&mut files_buf, &add).map_err(Error::from)?;
                        Ok(BlobRef {
                            offset: offset as u64,
                            kind: BlobRefKind::Local,
                        })
                    })
                    .transpose()?;
                Ok(Inode::new_file(
                    f.ino,
                    &f.md,
                    chunk_offset as u64,
                    additional_ref,
                )?)
            })
            .collect::<Result<Vec<Inode>>>()?,
    );

    let mut md_buf = Vec::<u8>::with_capacity(inodes_serial_size + dir_buf.len() + files_buf.len());
    serde_cbor::to_writer(&mut md_buf, &pfs_inodes)?;

    assert_eq!(md_buf.len(), inodes_serial_size);

    md_buf.append(&mut dir_buf);
    md_buf.append(&mut files_buf);

    let desc = oci.put_blob(md_buf.as_slice())?;
    let metadatas = [BlobRef {
        offset: 0,
        kind: BlobRefKind::Other {
            digest: desc.digest,
        },
    }]
    .to_vec();
    Ok(Rootfs { metadatas })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    use tempfile::tempdir;

    use format::InodeMode;

    #[test]
    fn test_fs_generation() {
        let dir = tempdir().unwrap();
        let image = Image::new(dir.path()).unwrap();

        // TODO: verify the hash value here since it's only one thing? problem is as we change the
        // encoding/add stuff to it, the hash will keep changing and we'll have to update the
        // test...
        //
        // but once all that's stabalized, we should verify the metadata hash too.
        let rootfs = build_initial_rootfs(Path::new("./test"), &image).unwrap();

        // there should be a blob that matches the hash of the test data, since it all gets input
        // as one chunk and there's only one file
        const FILE_DIGEST: &str =
            "d9e749d9367fc908876749d6502eb212fee88c9a94892fb07da5ef3ba8bc39ed";

        let md = fs::symlink_metadata(image.blob_path().join(FILE_DIGEST)).unwrap();
        assert!(md.is_file());

        let blob = image.open_blob(&rootfs.metadatas[0]).unwrap();
        let raw_inodes = blob
            .bytes()
            .take(inode_encoded_size(2))
            .collect::<io::Result<Vec<u8>>>()
            .unwrap();
        let inodes: Vec<Inode> = serde_cbor::from_reader(raw_inodes.as_slice()).unwrap();

        // we can at least deserialize inodes and they look sane
        assert_eq!(inodes.len(), 2);

        assert_eq!(inodes[0].ino, 1);
        assert_matches!(inodes[0].mode, InodeMode::Dir { .. });
        assert_eq!(inodes[0].uid, md.uid());
        assert_eq!(inodes[0].gid, md.gid());

        assert_eq!(inodes[1].ino, 2);
        assert_matches!(inodes[1].mode, InodeMode::Reg { .. });
        assert_eq!(inodes[1].uid, md.uid());
        assert_eq!(inodes[1].gid, md.gid());
    }
}