extern crate nix;
extern crate libc;

use super::{MemFile,
    MemFileWLock,
    MemFileRLock,
    MemFileWLockSlice,
    MemFileRLockSlice};

use self::libc::{
    pthread_rwlock_t,
    pthread_rwlock_init,
    pthread_rwlock_unlock,
    //pthread_rwlock_tryrdlock,
    //pthread_rwlock_trywrlock,
    pthread_rwlock_rdlock,
    pthread_rwlock_wrlock,

    /* Lock Attribute stuff */
    pthread_rwlockattr_t,
    pthread_rwlockattr_init,
    pthread_rwlockattr_setpshared,
    PTHREAD_PROCESS_SHARED,
};

use self::nix::sys::mman::{mmap, munmap, shm_open, shm_unlink, ProtFlags, MapFlags};
use self::nix::sys::stat::{fstat, FileStat, Mode};
use self::nix::fcntl::OFlag;
use self::nix::unistd::{close, ftruncate};

use std;
use std::path::PathBuf;
use std::os::raw::c_void;
use std::slice;
use std::os::unix::io::RawFd;
use std::ptr::{null_mut};
use std::mem::size_of;

type Result<T> = std::result::Result<T, Box<std::error::Error>>;

///This struct lives insides the shared memory
struct MemCtl {
    ///Lock controlling the access to the mapping
    rw_lock: pthread_rwlock_t,
}

pub struct MemMetadata {
    ///True if we created the mapping
    owner: bool,
    ///Name of mapping
    map_name: String,
    ///File descriptor from shm_open()
    map_fd: RawFd,
    ///Hold data to control the mapping (locks)
    map_ctl: *mut MemCtl,
    ///Holds the actual sizer of the mapping
    map_size: usize,
    ///Pointer to user data
    map_data: *mut c_void,
}

impl MemMetadata {

    /* Get Read Lock Impl */

    //Regular type
    pub fn rlock<T>(&self) -> MemFileRLock<T> {
        unsafe {
            //Acquire read lock
            pthread_rwlock_rdlock(&mut (*self.map_ctl).rw_lock);
            MemFileRLock {
                data: &(*(self.map_data as *mut T)),
                lock: &mut (*self.map_ctl).rw_lock as *mut _ as *mut c_void,
            }
        }
    }

    //Slice of type
    pub fn rlock_slice<T>(&self, start_offset: usize, num_elements:usize) -> MemFileRLockSlice<T> {
        unsafe {
            //Acquire read lock
            pthread_rwlock_rdlock(&mut (*self.map_ctl).rw_lock);
            MemFileRLockSlice {
                data: slice::from_raw_parts((self.map_data as usize + start_offset) as *const T, num_elements),
                lock: &mut (*self.map_ctl).rw_lock as *mut _ as *mut c_void,
            }
        }
    }

    /* Get Write Lock Impl */

    //Regular type
    pub fn wlock<T>(&mut self) -> MemFileWLock<T> {
        unsafe {
            //Acquire write lock
            pthread_rwlock_wrlock(&mut (*self.map_ctl).rw_lock);
            MemFileWLock {
                data: &mut (*(self.map_data as *mut T)),
                lock: &mut (*self.map_ctl).rw_lock as *mut _ as *mut c_void,
            }
        }
    }

    //Slice of type
    pub fn wlock_slice<T>(&mut self, start_offset: usize, num_elements:usize) -> MemFileWLockSlice<T> {
        unsafe{
            //acquire write lock
            pthread_rwlock_wrlock(&mut (*self.map_ctl).rw_lock);
            MemFileWLockSlice {
                data: slice::from_raw_parts_mut((self.map_data as usize + start_offset) as *mut T, num_elements),
                lock: &mut (*self.map_ctl).rw_lock as *mut _ as *mut c_void,
            }
        }
    }
}

impl Drop for MemMetadata {
    ///Takes care of properly closing the MemFile (munmap(), shmem_unlink(), close())
    fn drop(&mut self) {

        //Unmap memory
        if !self.map_ctl.is_null() {
            match unsafe {munmap(self.map_ctl as *mut _, self.map_size)} {
                Ok(_) => {
                    //println!("munmap()");
                },
                Err(e) => {
                    println!("Failed to unmap memory while dropping MemFile !");
                    println!("{}", e);
                },
            };
        }

        //Unlink shmem
        if self.map_fd != 0 {
            //unlink shmem if we created it
            if self.owner {
                match shm_unlink(self.map_name.as_str()) {
                    Ok(_) => {
                        //println!("shm_unlink()");
                    },
                    Err(e) => {
                        println!("Failed to shm_unlink while dropping MemFile !");
                        println!("{}", e);
                    },
                };
            }

            match close(self.map_fd) {
                Ok(_) => {
                    //println!("close()");
                },
                Err(e) => {
                    println!("Failed to close shmem fd while dropping MemFile !");
                    println!("{}", e);
                },
            };
        }
    }
}

//Opens an existing MemFile, shm_open()s it then mmap()s it
pub fn open(mut new_file: MemFile) -> Result<MemFile> {

    // Get the shmem path
    let shmem_path = match new_file.real_path {
        Some(ref path) => path.clone(),
        None => {
            panic!("Tried to open MemFile with no real_path");
        },
    };

    let mut meta: MemMetadata = MemMetadata {
        owner: new_file.owner,
        map_name: shmem_path,
        map_fd: 0,
        map_ctl: null_mut(),
        map_size: 0,
        map_data: null_mut(),
    };

    //Open shared memory
    meta.map_fd = match shm_open(
        meta.map_name.as_str(),
        OFlag::O_RDWR, //open for reading only
        Mode::S_IRUSR  //open for reading only
    ) {
        Ok(v) => v,
        Err(e) => return Err(From::from(format!("shm_open() failed with :\n{}", e))),
    };
    let file_stat: FileStat = match fstat(meta.map_fd) {
        Ok(v) => v,
        Err(e) => {
            return Err(From::from(e));
        }
    };

    let actual_size: usize = file_stat.st_size as usize;
    new_file.size = actual_size - size_of::<MemCtl>();

    //Map memory into our address space
    let map_addr: *mut c_void = match unsafe {
        mmap(null_mut(), //Desired addr
            actual_size, //size of mapping
            ProtFlags::PROT_READ|ProtFlags::PROT_WRITE, //Permissions on pages
            MapFlags::MAP_SHARED, //What kind of mapping
            meta.map_fd, //fd
            0   //Offset into fd
        )
    } {
        Ok(v) => v as *mut c_void,
        Err(e) => return Err(From::from(format!("mmap() failed with :\n{}", e))),
    };

    //Create control structures for the mapping
    meta.map_ctl = map_addr as *mut _;
    //Save the actual size of the mapping
    meta.map_size = actual_size;
    //Init pointer to user data
    meta.map_data = (map_addr as usize + size_of::<MemCtl>()) as *mut c_void;
    //This meta struct is now link to the MemFile
    new_file.meta = Some(meta);


    Ok(new_file)
}

//Creates a new MemFile, shm_open()s it then mmap()s it
pub fn create(mut new_file: MemFile) -> Result<MemFile> {

    // real_path is either :
    // 1. Specified directly
    // 2. Needs to be generated (link_file needs to exist)
    let real_path: String = match new_file.real_path {
        Some(ref path) => path.clone(),
        None => {
            //We dont have a real path and a link file wasn created
            if let Some(ref file_path) = new_file.link_path {
                if !file_path.is_file() {
                    return Err(From::from("os_impl::create() on a link but not link file exists..."));
                }

                //Get unique name for shmem object
                let abs_disk_path: PathBuf = file_path.canonicalize()?;
                let chars = abs_disk_path.to_string_lossy();
                let mut unique_name: String = String::with_capacity(chars.len());
                let mut chars = chars.chars();
                chars.next();
                unique_name.push('/');
                for c in chars {
                    match c {
                        '/' | '.' => unique_name.push('_'),
                        v => unique_name.push(v),
                    };
                }
                unique_name
            } else {
                //lib.rs shouldnt call us without either real_path or link_path set
                panic!("Trying to create MemFile without any name");
            }
        }
    };

    //Create shared memory
    //TODO : Handle "File exists" errors when creating MemFile with new_file.link_path.is_some()
    //       When new_file.link_path.is_some(), we can figure out a real_path that doesnt collide with another
    //       and stick it in the link_file
    let shmem_fd = match shm_open(
        real_path.as_str(), //Unique name that usualy pops up in /dev/shm/
        OFlag::O_CREAT|OFlag::O_EXCL|OFlag::O_RDWR, //create exclusively (error if collision) and read/write to allow resize
        Mode::S_IRUSR|Mode::S_IWUSR //Permission allow user+rw
    ) {
        Ok(v) => v,
        Err(e) => return Err(From::from(format!("shm_open() failed with :\n{}", e))),
    };

    new_file.real_path = Some(real_path.clone());
    let mut meta: MemMetadata = MemMetadata {
        owner: new_file.owner,
        map_name: real_path,
        map_fd: shmem_fd,
        map_ctl: null_mut(),
        map_size: 0,
        map_data: null_mut(),
    };

    //increase size to requested size + meta
    let actual_size: usize = new_file.size + size_of::<MemCtl>();

    match ftruncate(meta.map_fd, actual_size as i64) {
        Ok(_) => {},
        Err(e) => return Err(From::from(format!("ftruncate() failed with :\n{}", e))),
    };

    //Map memory into our address space
    let map_addr: *mut c_void = match unsafe {
        mmap(null_mut(), //Desired addr
            actual_size, //size of mapping
            ProtFlags::PROT_READ|ProtFlags::PROT_WRITE, //Permissions on pages
            MapFlags::MAP_SHARED, //What kind of mapping
            meta.map_fd, //fd
            0   //Offset into fd
        )
    } {
        Ok(v) => v as *mut c_void,
        Err(e) => return Err(From::from(format!("mmap() failed with :\n{}", e))),
    };

    //Initialise our metadata struct
    {
        //Create control structures for the mapping
        meta.map_ctl = map_addr as *mut _;
        //Save the actual size of the mapping
        meta.map_size = actual_size;

        let mut lock_attr: [u8; size_of::<pthread_rwlockattr_t>()] = [0; size_of::<pthread_rwlockattr_t>()];

        unsafe {
            //Set the PTHREAD_PROCESS_SHARED attribute on our rwlock
            pthread_rwlockattr_init(lock_attr.as_mut_ptr() as *mut pthread_rwlockattr_t);
            pthread_rwlockattr_setpshared(lock_attr.as_mut_ptr() as *mut pthread_rwlockattr_t, PTHREAD_PROCESS_SHARED);
            //Init the rwlock
            pthread_rwlock_init(&mut (*meta.map_ctl).rw_lock, lock_attr.as_mut_ptr() as *mut pthread_rwlockattr_t);
        }

        //Init pointer to user data
        meta.map_data = (map_addr as usize + size_of::<MemCtl>()) as *mut c_void;
    }

    //Link the finalized metadata to the MemFile
    new_file.meta = Some(meta);

    Ok(new_file)
}

//Returns a read lock to the shared memory
pub fn rlock<T>(meta: &MemMetadata) -> MemFileRLock<T> {
    return meta.rlock();
}
//Returns an exclusive read/write lock to the shared memory
pub fn wlock<T>(meta: &mut MemMetadata) -> MemFileWLock<T> {
    return meta.wlock();
}
//Returns a read lock to the shared memory as a slice
pub fn rlock_slice<T>(meta: &MemMetadata, start_offset: usize, num_elements:usize) -> MemFileRLockSlice<T> {
    return meta.rlock_slice(start_offset, num_elements);
}
///Returns an exclusive read/write lock to the shared memory as a slice
pub fn wlock_slice<T>(meta: &mut MemMetadata, start_offset: usize, num_elements:usize) -> MemFileWLockSlice<T> {
    return meta.wlock_slice(start_offset, num_elements);
}

//On linux, you call the same function whether you held read or write
pub fn read_unlock(lock_ptr: *mut c_void) { unlock(lock_ptr); }
pub fn write_unlock(lock_ptr: *mut c_void) { unlock(lock_ptr); }
pub fn unlock(lock_ptr: *mut c_void) {
    unsafe {pthread_rwlock_unlock(lock_ptr as *mut pthread_rwlock_t)};
}
