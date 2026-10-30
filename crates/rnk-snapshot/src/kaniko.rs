//! kaniko mode: the root is walked and hashed instead of mounted
//! (`design/kaniko-mode.md`). Built so far: the ignore list.

mod ignore;

pub use ignore::{IgnoreSpec, MountInfo, ignore_list, parse_mountinfo, read_mountinfo};
