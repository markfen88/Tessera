//! memfd creation, sealing and attach-time checks (safe code over `nix`).

use std::os::fd::OwnedFd;

use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::libc;
use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::sys::stat::fstat;
use nix::unistd::ftruncate;

use crate::error::Error;

/// Seals every mapping we accept must carry: the size can never change
/// (otherwise a peer could `ftruncate` and make our accesses `SIGBUS`) and no
/// further seals can be added or removed.
pub(crate) fn required_seals() -> SealFlag {
    SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW | SealFlag::F_SEAL_SEAL
}

/// Creates a zero-filled, sealable, close-on-exec memfd of `len` bytes.
pub(crate) fn create(name: &str, len: u64) -> Result<OwnedFd, Error> {
    let fd = memfd_create(name, MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING)?;
    let len = libc::off_t::try_from(len)
        .map_err(|_| Error::Io(std::io::Error::from(std::io::ErrorKind::InvalidInput)))?;
    ftruncate(&fd, len)?;
    Ok(fd)
}

/// Adds `seals` (call after the owner has made its writable mapping when
/// using `F_SEAL_FUTURE_WRITE`).
pub(crate) fn add_seals(fd: &OwnedFd, seals: SealFlag) -> Result<(), Error> {
    fcntl(fd, FcntlArg::F_ADD_SEALS(seals))?;
    Ok(())
}

/// Checks an fd received from a peer before mapping it: it must carry
/// [`required_seals`] (checked first), be a
/// regular file (memfds are), its size must lie in `[min_len, max_len]`, and
/// it must carry [`required_seals`]. Returns the size.
///
/// `F_GET_SEALS` fails with `EINVAL` on files that do not support sealing,
/// which doubles as a type check.
pub(crate) fn check_attachable(fd: &OwnedFd, min_len: u64, max_len: u64) -> Result<u64, Error> {
    // Seals first: until SHRINK/GROW are verified the peer could still resize
    // the file, so a size read before this check would prove nothing.
    let have = fcntl(fd, FcntlArg::F_GET_SEALS)?;
    let need = required_seals().bits();
    let have = u32::try_from(have).unwrap_or(0);
    if have & need as u32 != need as u32 {
        return Err(Error::NotSealed {
            have,
            need: need as u32,
        });
    }
    let st = fstat(fd)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(crate::error::LayoutError("descriptor is not a regular memfd").into());
    }
    let size =
        u64::try_from(st.st_size).map_err(|_| crate::error::LayoutError("negative file size"))?;
    if size < min_len {
        return Err(crate::error::LayoutError("mapping smaller than its header").into());
    }
    if size > max_len {
        return Err(crate::error::LayoutError("mapping larger than the configured limit").into());
    }
    Ok(size)
}
