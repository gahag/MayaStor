//! High-level storage pool json-rpc methods.
//!
//! They provide abstraction on top of aio bdev, lvol store, etc and export
//! simple to use json-rpc methods for managing pools.

use crate::{
    bdev::{bdev_lookup_by_name, Bdev},
    executor::{cb_arg, done_cb},
    jsonrpc::{jsonrpc_register, Code, JsonRpcError, Result},
    replica::ReplicaIter,
};
use futures::{
    channel::oneshot,
    future::{self, FutureExt},
};
use rpc::jsonrpc as jsondata;
use spdk_sys::{
    create_aio_bdev,
    delete_aio_bdev,
    lvol_store_bdev,
    spdk_bs_free_cluster_count,
    spdk_bs_get_cluster_size,
    spdk_bs_total_data_cluster_count,
    spdk_lvol_store,
    vbdev_get_lvol_store_by_name,
    vbdev_get_lvs_bdev_by_lvs,
    vbdev_lvol_store_first,
    vbdev_lvol_store_next,
    vbdev_lvs_create,
    vbdev_lvs_destruct,
    vbdev_lvs_examine,
    LVS_CLEAR_WITH_UNMAP,
};
use std::{
    ffi::{c_void, CStr, CString},
    os::raw::c_char,
};

/// Wrapper for create aio bdev C function
fn create_base_bdev(file: &str, block_size: u32) -> Result<()> {
    debug!("Creating aio bdev {} ...", file);
    let cstr_file = CString::new(file).unwrap();
    let rc = unsafe {
        create_aio_bdev(cstr_file.as_ptr(), cstr_file.as_ptr(), block_size)
    };
    if rc != 0 {
        Err(JsonRpcError::new(
            Code::InvalidParams,
            "AIO bdev already exists or parameters are invalid",
        ))
    } else {
        info!("aio bdev {} was created", file);
        Ok(())
    }
}

/// Callback called from SPDK for pool create and import methods.
extern "C" fn pool_done_cb(
    sender_ptr: *mut c_void,
    _lvs: *mut spdk_lvol_store,
    errno: i32,
) {
    let sender =
        unsafe { Box::from_raw(sender_ptr as *mut oneshot::Sender<i32>) };
    sender.send(errno).expect("Receiver is gone");
}

/// Structure representing a pool which comprises lvol store and
/// underlaying bdev.
///
/// Note about safety: The structure wraps raw C pointers from SPDK.
/// It is safe to use only in synchronous context. If you keep Pool for
/// longer than that then something else can run on reactor_0 inbetween
/// which may destroy the pool and invalidate the pointers!
pub struct Pool {
    lvs_ptr: *mut spdk_lvol_store,
    lvs_bdev_ptr: *mut lvol_store_bdev,
}

impl Pool {
    /// Easy converter from raw pointer to Pool object
    unsafe fn from_ptr(ptr: *mut lvol_store_bdev) -> Pool {
        Pool {
            lvs_ptr: (*ptr).lvs,
            lvs_bdev_ptr: ptr,
        }
    }

    /// Look up existing pool by name
    pub fn lookup(name: &str) -> Option<Self> {
        let name = CString::new(name).unwrap();
        let lvs_ptr = unsafe { vbdev_get_lvol_store_by_name(name.as_ptr()) };
        if lvs_ptr.is_null() {
            return None;
        }
        let lvs_bdev_ptr = unsafe { vbdev_get_lvs_bdev_by_lvs(lvs_ptr) };
        if lvs_bdev_ptr.is_null() {
            // can happen if lvs is being destroyed
            return None;
        }
        Some(Self {
            lvs_ptr,
            lvs_bdev_ptr,
        })
    }

    /// Get base bdev for the pool (in our case AIO bdev).
    pub fn get_name(&self) -> &str {
        unsafe {
            let lvs = &*self.lvs_ptr;
            CStr::from_ptr(&lvs.name as *const c_char).to_str().unwrap()
        }
    }

    /// Get base bdev for the pool (in our case AIO bdev).
    pub fn get_base_bdev(&self) -> Bdev {
        let base_bdev_ptr = unsafe { (*self.lvs_bdev_ptr).bdev };
        base_bdev_ptr.into()
    }

    /// Get capacity of the pool in bytes.
    pub fn get_capacity(&self) -> u64 {
        unsafe {
            let lvs = &*self.lvs_ptr;
            let cluster_size = spdk_bs_get_cluster_size(lvs.blobstore);
            let total_clusters =
                spdk_bs_total_data_cluster_count(lvs.blobstore);
            total_clusters * cluster_size
        }
    }

    /// Get free space in the pool in bytes.
    pub fn get_free(&self) -> u64 {
        unsafe {
            let lvs = &*self.lvs_ptr;
            let cluster_size = spdk_bs_get_cluster_size(lvs.blobstore);
            spdk_bs_free_cluster_count(lvs.blobstore) * cluster_size
        }
    }

    /// Return raw pointer to spdk lvol store structure
    pub fn as_ptr(&self) -> *mut spdk_lvol_store {
        self.lvs_ptr
    }

    /// Create a pool on base bdev
    pub async fn create<'a>(name: &'a str, disk: &'a str) -> Result<Pool> {
        let base_bdev = match bdev_lookup_by_name(disk) {
            Some(bdev) => bdev,
            None => {
                return Err(JsonRpcError::new(
                    Code::NotFound,
                    format!("Base bdev {} does not exist", disk),
                ))
            }
        };
        let pool_name = CString::new(name).unwrap();
        let (sender, receiver) = oneshot::channel::<i32>();
        let rc = unsafe {
            vbdev_lvs_create(
                base_bdev.as_ptr(),
                pool_name.as_ptr(),
                0,
                // Clearing pool is not strictly necessary so do it only if it
                // can be done fast (unmap) otherwise it is a
                // noop.
                LVS_CLEAR_WITH_UNMAP,
                Some(pool_done_cb),
                cb_arg(sender),
            )
        };
        // TODO: free sender
        if rc < 0 {
            return Err(JsonRpcError::new(
                Code::InvalidParams,
                format!("Failed to create the pool {}", name),
            ));
        }

        let lvs_errno = receiver.await.expect("Cancellation is not supported");
        if lvs_errno != 0 {
            return Err(JsonRpcError::new(
                Code::InvalidParams,
                format!(
                    "Failed to create the pool {} (errno={})",
                    name, lvs_errno
                ),
            ));
        }

        match Pool::lookup(&name) {
            Some(pool) => {
                info!("The pool {} has been created", name);
                Ok(pool)
            }
            None => Err(JsonRpcError::new(
                Code::InternalError,
                format!("The pool {} disappeared", name),
            )),
        }
    }

    /// Import the pool from a disk
    pub async fn import<'a>(name: &'a str, disk: &'a str) -> Result<Pool> {
        let base_bdev = match bdev_lookup_by_name(disk) {
            Some(bdev) => bdev,
            None => {
                return Err(JsonRpcError::new(
                    Code::InternalError,
                    format!("Base bdev {} does not exist", disk),
                ))
            }
        };

        let (sender, receiver) = oneshot::channel::<i32>();

        debug!("Trying to import pool {}", name);

        unsafe {
            vbdev_lvs_examine(
                base_bdev.as_ptr(),
                Some(pool_done_cb),
                cb_arg(sender),
            );
        }
        let lvs_errno = receiver.await.expect("Cancellation is not supported");
        if lvs_errno == 0 {
            // could be that a pool with a different name was imported
            match Pool::lookup(&name) {
                Some(pool) => {
                    info!("The pool {} has been imported", name);
                    Ok(pool)
                }
                None => Err(JsonRpcError::new(
                    Code::InvalidParams,
                    format!("The device {} hosts another pool", disk),
                )),
            }
        } else {
            Err(JsonRpcError::new(
                Code::InternalError,
                format!(
                    "Failed to import the pool {} (errno={})",
                    name, lvs_errno
                ),
            ))
        }
    }

    /// Destroy the pool
    pub async fn destroy(self) -> Result<()> {
        let name = self.get_name().to_string();
        let base_bdev_name = self.get_base_bdev().name();

        debug!("Destroying the pool {}", name);

        // unshare all replicas on the pool at first
        for replica in ReplicaIter::new() {
            if replica.get_pool_name() == name {
                replica.unshare().await?;
            }
        }

        // we will destroy lvol store now
        let (sender, receiver) = oneshot::channel::<i32>();
        unsafe {
            vbdev_lvs_destruct(self.lvs_ptr, Some(done_cb), cb_arg(sender));
        }
        let lvs_errno = receiver.await.expect("Cancellation is not supported");
        if lvs_errno != 0 {
            return Err(JsonRpcError::new(
                Code::InternalError,
                format!(
                    "Failed to destroy pool {} (errno={})",
                    name, lvs_errno
                ),
            ));
        }

        // we will destroy base bdev now
        let base_bdev = match bdev_lookup_by_name(&base_bdev_name) {
            Some(bdev) => bdev,
            None => {
                // it's not an error if the base bdev disappeared but it is
                // weird
                warn!(
                    "Base bdev {} disappeared while destroying the pool {}",
                    base_bdev_name, name
                );
                return Ok(());
            }
        };
        let (sender, receiver) = oneshot::channel::<i32>();
        unsafe {
            delete_aio_bdev(base_bdev.as_ptr(), Some(done_cb), cb_arg(sender));
        }
        let bdev_errno = receiver.await.expect("Cancellation is not supported");
        if bdev_errno != 0 {
            Err(JsonRpcError::new(
                Code::InternalError,
                format!(
                    "Failed to destroy base bdev {} for the pool {} (errno={})",
                    base_bdev_name, name, bdev_errno
                ),
            ))
        } else {
            info!(
                "The pool {} and base bdev {} have been destroyed",
                name, base_bdev_name
            );
            Ok(())
        }
    }
}

/// Iterator over available storage pools.
#[derive(Default)]
pub struct PoolsIter {
    lvs_bdev_ptr: Option<*mut lvol_store_bdev>,
}

impl PoolsIter {
    pub fn new() -> Self {
        Self {
            lvs_bdev_ptr: None,
        }
    }
}

impl Iterator for PoolsIter {
    type Item = Pool;

    fn next(&mut self) -> Option<Self::Item> {
        let next_ptr = match self.lvs_bdev_ptr {
            None => unsafe { vbdev_lvol_store_first() },
            Some(ptr) => {
                assert!(!ptr.is_null());
                unsafe { vbdev_lvol_store_next(ptr) }
            }
        };
        self.lvs_bdev_ptr = Some(next_ptr);

        if next_ptr.is_null() {
            None
        } else {
            Some(unsafe { Pool::from_ptr(next_ptr) })
        }
    }
}

fn list_pools() -> Vec<jsondata::Pool> {
    let mut pools = Vec::new();

    for pool in PoolsIter::new() {
        pools.push(jsondata::Pool {
            name: pool.get_name().to_owned(),
            disks: vec![pool.get_base_bdev().name()],
            // TODO: figure out how to detect state of pool
            state: "online".to_owned(),
            capacity: pool.get_capacity(),
            used: pool.get_capacity() - pool.get_free(),
        });
    }
    pools
}

/// Register storage pool json-rpc methods.
pub fn register_pool_methods() {
    // Joining create and import together is questionable and we might split
    // the two operations in future. However not until cache config file
    // feature is implemented and requirements become clear.
    jsonrpc_register(
        "create_or_import_pool",
        |args: jsondata::CreateOrImportPoolArgs| {
            let fut = async move {
                // TODO: support RAID-0 devices
                if args.disks.len() != 1 {
                    return Err(JsonRpcError::new(
                        Code::InvalidParams,
                        "Invalid number of disks specified",
                    ));
                }

                if Pool::lookup(&args.name).is_some() {
                    return Err(JsonRpcError::new(
                        Code::AlreadyExists,
                        format!("The pool {} already exists", args.name),
                    ));
                }

                // TODO: We would like to check if the disk is in use, but there
                // is no easy way how to get this info using available api.
                let disk = &args.disks[0];
                if bdev_lookup_by_name(disk).is_some() {
                    return Err(JsonRpcError::new(
                        Code::InvalidParams,
                        format!("Base bdev {} already exists", disk),
                    ));
                }
                if let Err(err) =
                    create_base_bdev(disk, args.block_size.unwrap_or(4096))
                {
                    return Err(err);
                };

                if Pool::import(&args.name, disk).await.is_ok() {
                    return Ok(());
                }
                match Pool::create(&args.name, disk).await {
                    Ok(_) => Ok(()),
                    Err(err) => Err(err),
                }
            };
            fut.boxed_local()
        },
    );

    jsonrpc_register("destroy_pool", |args: jsondata::DestroyPoolArgs| {
        let fut = async move {
            let pool = match Pool::lookup(&args.name) {
                Some(p) => p,
                None => {
                    return Err(JsonRpcError::new(
                        Code::NotFound,
                        format!("The pool {} does not exist", args.name),
                    ));
                }
            };
            pool.destroy().await
        };
        fut.boxed_local()
    });

    jsonrpc_register::<(), _, _>("list_pools", |_| {
        future::ok(list_pools()).boxed_local()
    });
}
