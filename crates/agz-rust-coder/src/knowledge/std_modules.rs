//! Small standard-library lookup table used before external crate lookup.

/// A standard-library name which is commonly mistaken for an external crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdModuleEntry {
    /// The short name accepted by the lookup.
    pub name: &'static str,
    /// The canonical standard-library path.
    pub module: &'static str,
    /// A short description of the covered capability.
    pub members: &'static str,
}

/// Names which should not be added to `Cargo.toml` as dependencies.
pub const STD_MODULES: &[StdModuleEntry] = &[
    StdModuleEntry {
        name: "std",
        module: "std",
        members: "the Rust standard library",
    },
    StdModuleEntry {
        name: "HashMap",
        module: "std::collections::HashMap",
        members: "hash maps",
    },
    StdModuleEntry {
        name: "HashSet",
        module: "std::collections::HashSet",
        members: "hash sets",
    },
    StdModuleEntry {
        name: "BTreeMap",
        module: "std::collections::BTreeMap",
        members: "ordered maps",
    },
    StdModuleEntry {
        name: "BTreeSet",
        module: "std::collections::BTreeSet",
        members: "ordered sets",
    },
    StdModuleEntry {
        name: "VecDeque",
        module: "std::collections::VecDeque",
        members: "double-ended queues",
    },
    StdModuleEntry {
        name: "BinaryHeap",
        module: "std::collections::BinaryHeap",
        members: "priority queues",
    },
    StdModuleEntry {
        name: "LinkedList",
        module: "std::collections::LinkedList",
        members: "linked lists",
    },
    StdModuleEntry {
        name: "collections",
        module: "std::collections",
        members: "HashMap, HashSet, VecDeque, BTreeMap, BinaryHeap",
    },
    StdModuleEntry {
        name: "Vec",
        module: "std::vec::Vec",
        members: "growable arrays",
    },
    StdModuleEntry {
        name: "String",
        module: "std::string::String",
        members: "owned UTF-8 strings",
    },
    StdModuleEntry {
        name: "str",
        module: "std::str",
        members: "string slices",
    },
    StdModuleEntry {
        name: "Option",
        module: "std::option::Option",
        members: "optional values",
    },
    StdModuleEntry {
        name: "Result",
        module: "std::result::Result",
        members: "fallible results",
    },
    StdModuleEntry {
        name: "io",
        module: "std::io",
        members: "stdin/stdout/stderr, Read, Write, BufReader, BufWriter",
    },
    StdModuleEntry {
        name: "fs",
        module: "std::fs",
        members: "file and directory operations",
    },
    StdModuleEntry {
        name: "path",
        module: "std::path",
        members: "Path, PathBuf",
    },
    StdModuleEntry {
        name: "env",
        module: "std::env",
        members: "environment variables and args",
    },
    StdModuleEntry {
        name: "process",
        module: "std::process",
        members: "Command, exit",
    },
    StdModuleEntry {
        name: "thread",
        module: "std::thread",
        members: "spawn, JoinHandle",
    },
    StdModuleEntry {
        name: "sync",
        module: "std::sync",
        members: "Arc, Mutex, RwLock, mpsc, Once, Barrier",
    },
    StdModuleEntry {
        name: "mpsc",
        module: "std::sync::mpsc",
        members: "channels",
    },
    StdModuleEntry {
        name: "Mutex",
        module: "std::sync::Mutex",
        members: "sync mutex (never hold across .await)",
    },
    StdModuleEntry {
        name: "RwLock",
        module: "std::sync::RwLock",
        members: "reader-writer lock",
    },
    StdModuleEntry {
        name: "Arc",
        module: "std::sync::Arc",
        members: "shared ownership (use Arc::clone)",
    },
    StdModuleEntry {
        name: "cell",
        module: "std::cell",
        members: "Cell, RefCell (interior mutability - last resort)",
    },
    StdModuleEntry {
        name: "fmt",
        module: "std::fmt",
        members: "Display, Debug, formatting",
    },
    StdModuleEntry {
        name: "cmp",
        module: "std::cmp",
        members: "Ord, Eq, ordering",
    },
    StdModuleEntry {
        name: "iter",
        module: "std::iter",
        members: "iterator adaptors",
    },
    StdModuleEntry {
        name: "net",
        module: "std::net",
        members: "TcpStream, TcpListener, UdpSocket",
    },
    StdModuleEntry {
        name: "time",
        module: "std::time",
        members: "Duration, Instant, SystemTime",
    },
    StdModuleEntry {
        name: "error",
        module: "std::error",
        members: "Error trait",
    },
    StdModuleEntry {
        name: "num",
        module: "std::num",
        members: "numeric types and helpers",
    },
    StdModuleEntry {
        name: "mem",
        module: "std::mem",
        members: "take, replace, swap, size_of",
    },
    StdModuleEntry {
        name: "convert",
        module: "std::convert",
        members: "From, Into, TryFrom",
    },
    StdModuleEntry {
        name: "ops",
        module: "std::ops",
        members: "operator traits, Range",
    },
    StdModuleEntry {
        name: "rc",
        module: "std::rc",
        members: "Rc (single-threaded shared ownership)",
    },
    StdModuleEntry {
        name: "char",
        module: "std::char",
        members: "character helpers",
    },
    StdModuleEntry {
        name: "ascii",
        module: "std::ascii",
        members: "ASCII helpers",
    },
];

/// Policy text used by the crate lookup result and coding guidance.
pub const STD_POLICY: &str = "If a required capability is covered by the standard library, use std and do not add an external crate.";

/// Looks up a standard-library name case-insensitively.
pub fn std_module_lookup(name: &str) -> Option<&'static StdModuleEntry> {
    let mut canonical = name.trim();
    for prefix in ["crate::", "std::"] {
        if canonical.len() >= prefix.len()
            && canonical
                .get(..prefix.len())
                .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
        {
            canonical = &canonical[prefix.len()..];
            break;
        }
    }
    STD_MODULES
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(canonical))
}
