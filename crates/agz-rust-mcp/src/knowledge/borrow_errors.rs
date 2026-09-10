#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowHint {
    pub what: &'static str,
    pub wrong_reaction: &'static str,
    pub correct_fix: &'static str,
}

pub const BORROW_HINTS: &[(&str, BorrowHint)] = &[
    (
        "E0382",
        BorrowHint {
            what: "Use of moved value.",
            wrong_reaction: "Do not sprinkle .clone() everywhere.",
            correct_fix: "Ask who should own the data. Borrow (&value), restructure, or clone only when a second independent owner truly exists.",
        },
    ),
    (
        "E0502",
        BorrowHint {
            what: "Cannot borrow as mutable because it is also borrowed as immutable.",
            wrong_reaction: "Do not clone the immutable part.",
            correct_fix: "Finish reading before writing: end the immutable borrow (scope or reorder statements) before mutating.",
        },
    ),
    (
        "E0499",
        BorrowHint {
            what: "Cannot borrow as mutable more than once at a time.",
            wrong_reaction: "Do not reach for RefCell.",
            correct_fix: "Split the borrows: split_at_mut, separate scopes, pass individual fields instead of the whole struct, or destructure.",
        },
    ),
    (
        "E0505",
        BorrowHint {
            what: "Cannot move out of a value because it is borrowed.",
            wrong_reaction: "Do not force a move.",
            correct_fix: "Drop the borrow first, or clone the moved part with a justification.",
        },
    ),
    (
        "E0515",
        BorrowHint {
            what: "Cannot return a reference to a local variable.",
            wrong_reaction: "Do not Box::leak.",
            correct_fix: "Return the owned value (String, Vec, Box) instead of a reference.",
        },
    ),
    (
        "E0597",
        BorrowHint {
            what: "Borrowed value does not live long enough.",
            wrong_reaction: "Do not slap on 'static.",
            correct_fix: "Extend the value's scope, return owned data, or add a precise lifetime; 'static restricts the API to owned types.",
        },
    ),
    (
        "E0716",
        BorrowHint {
            what: "Temporary value dropped while borrowed.",
            wrong_reaction: "Do not leak it.",
            correct_fix: "Bind the temporary to a named variable with let. Note: edition 2024 changed temporary lifetimes in tail expressions.",
        },
    ),
    (
        "E0106",
        BorrowHint {
            what: "Missing lifetime specifier.",
            wrong_reaction: "Do not add 'a and hope.",
            correct_fix: "Elision usually suffices; write a named lifetime only when a type stores a borrow or output origin is ambiguous.",
        },
    ),
    (
        "E0507",
        BorrowHint {
            what: "Cannot move out of borrowed content.",
            wrong_reaction: "Do not clone by default.",
            correct_fix: "Should the caller pass ownership? Use &ref patterns, std::mem::take, or clone with a justification.",
        },
    ),
    (
        "E0596",
        BorrowHint {
            what: "Cannot borrow immutable variable as mutable.",
            wrong_reaction: "Do not wrap in Cell.",
            correct_fix: "Declare the binding as mut: let mut x = ...",
        },
    ),
];

pub fn hint_for(code: &str) -> Option<&'static BorrowHint> {
    BORROW_HINTS
        .iter()
        .find_map(|(candidate, hint)| (*candidate == code).then_some(hint))
}

pub const EXPLAIN_ADVICE: &str =
    "Run `rustc --explain <CODE>` for the official explanation of any listed error code.";
