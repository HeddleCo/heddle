// HeddleFSKit-Bridging.h
//
// GENERATED FROM `crates/mount/src/fskit/c_abi.rs` BY cbindgen.
// DO NOT EDIT BY HAND — re-run `cargo build -p mount --features fskit`.
//
// This is the C ABI surface that the Rust `fskit` shell calls into.
// The Swift implementation lives in `HeddleFSKit.swift`; the
// authoritative Rust declarations live in `crates/mount/src/fskit/c_abi.rs`.
// A change in Rust regenerates this header on the next build, and
// any drift between the Rust ABI and Swift's call sites becomes a
// `swiftc` compile error rather than runtime undefined behaviour.

#ifndef HEDDLE_FSKIT_BRIDGING_H
#define HEDDLE_FSKIT_BRIDGING_H

#include <stdint.h>
#include <stddef.h>

typedef void *HeddleFSKitSessionHandle;

typedef int32_t (*HeddleLookupCallback)(void *user_data,
                                        uint64_t parent_inode,
                                        const char *name_utf8,
                                        uint64_t *out_child_inode,
                                        uint32_t *out_unix_mode,
                                        uint64_t *out_size);

typedef int32_t (*HeddleGetattrCallback)(void *user_data,
                                         uint64_t inode,
                                         uint32_t *out_unix_mode,
                                         uint64_t *out_size,
                                         uint32_t *out_nlink);

typedef int32_t (*HeddleReadCallback)(void *user_data,
                                      uint64_t inode,
                                      uint64_t offset,
                                      uint8_t *buffer,
                                      uint64_t buffer_capacity,
                                      uint64_t *out_bytes_read);

typedef int32_t (*HeddleWriteCallback)(void *user_data,
                                       uint64_t inode,
                                       uint64_t offset,
                                       const uint8_t *data,
                                       uint64_t data_len,
                                       uint64_t *out_bytes_written);

typedef int32_t (*HeddleEnumerateEmit)(void *emit_user_data,
                                       uint64_t child_inode,
                                       const char *child_name_utf8,
                                       uint32_t unix_mode,
                                       uint64_t size);

typedef int32_t (*HeddleEnumerateCallback)(void *user_data,
                                           uint64_t dir_inode,
                                           void *emit_user_data,
                                           HeddleEnumerateEmit emit);

typedef int32_t (*HeddleFlushCallback)(void *user_data, uint64_t inode);

typedef void (*HeddleDropCallback)(void *user_data);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

extern HeddleFSKitSessionHandle heddle_fskit_session_new(void *user_data,
                                                         HeddleLookupCallback lookup,
                                                         HeddleGetattrCallback getattr,
                                                         HeddleReadCallback read,
                                                         HeddleWriteCallback write,
                                                         HeddleEnumerateCallback enumerate,
                                                         HeddleFlushCallback flush,
                                                         HeddleDropCallback drop);

extern int32_t heddle_fskit_session_mount(HeddleFSKitSessionHandle handle,
                                          const char *mountpoint_utf8);

extern int32_t heddle_fskit_session_unmount(HeddleFSKitSessionHandle handle);

extern void heddle_fskit_session_free(HeddleFSKitSessionHandle handle);

extern int32_t heddle_fskit_is_available(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* HEDDLE_FSKIT_BRIDGING_H */
