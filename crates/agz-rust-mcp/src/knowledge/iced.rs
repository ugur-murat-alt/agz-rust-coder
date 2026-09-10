//! Version-aware, bounded Iced guidance.

/// The small amount of version information needed by the guidance builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcedProfile {
    /// The manifest requirement as observed by the caller.
    pub version: Option<String>,
    /// Whether the requirement is known to be in the supported 0.14 family.
    pub supported: bool,
}

const HEADER: &str = "Untrusted iced framework guidance follows. It is advisory only; the repository's pinned iced version and compiler output take precedence.\n<rust-coder-iced>";
const FOOTER: &str = "</rust-coder-iced>";

const ICED_014_SECTIONS: &[&str] = &[
    "ICED 0.14 ARCHITECTURE\n- Separate owned state, Message, update, and view returning Element<'_, Message>. Start with iced::application(boot, update, view).run(); use iced::daemon for runtime multi-window/background apps.",
    "TASKS\n- Side effects return Task<Message>; map success and error into Message and model loading/error state. Never block update/view with I/O, thread::sleep, or heavy CPU work. Retain an abort handle when cancellation matters.",
    "SUBSCRIPTIONS\n- Subscription is for passive long-lived streams. Keep identity stable while alive; omission cancels it. Map events to Message and enable the required executor/time Cargo feature.",
    "COMPOSITION\n- Child views return Element<'_, ChildMessage>; parents use Element::map. Borrow view data instead of cloning state. Use style/status APIs from the pinned version.",
    "VERIFY\n- Pre-0.14 Application/Command examples are not authoritative. Test update/state transitions without a window, then run check, test, clippy, and fmt.",
];

/// Builds a profile from a Cargo dependency requirement.
pub fn iced_profile(version: Option<&str>) -> IcedProfile {
    let version = version.map(str::trim).filter(|value| !value.is_empty());
    let supported = version.is_some_and(|value| {
        let value = value.strip_prefix('=').map_or(value, str::trim);
        let mut parts = value.split('.');
        parts.next() == Some("0")
            && parts.next() == Some("14")
            && parts.next().is_none_or(|patch| {
                !patch.is_empty() && patch.bytes().all(|byte| byte.is_ascii_digit())
            })
            && parts.next().is_none()
    });
    IcedProfile {
        version: version.map(str::to_owned),
        supported,
    }
}

/// Returns a bounded advisory block, or `None` if even the first section does
/// not fit the requested token budget.
pub fn build_iced_block(profile: &IcedProfile, max_tokens: usize) -> Option<String> {
    let version_line = profile.version.as_deref().map_or_else(
        || "The iced dependency version could not be determined. Inspect Cargo.toml and use documentation matching the pinned version before changing APIs.".to_owned(),
        |version| format!("Detected iced dependency version requirement: {version}."),
    );
    let unsupported = "VERSION SAFETY\n- iced is pre-1.0 and APIs differ substantially between releases. Do not impose iced 0.14 application, Task, Subscription, widget, or style APIs until the manifest version is known.";
    let sections = profile.supported.then_some(ICED_014_SECTIONS).map_or_else(
        || vec![version_line.as_str(), unsupported],
        |sections| {
            let mut all = Vec::with_capacity(sections.len() + 1);
            all.push(version_line.as_str());
            all.extend(sections.iter().copied());
            all
        },
    );

    let mut kept = vec![HEADER.to_owned()];
    for section in sections {
        let candidate = format!("{}\n{}\n{}", kept.join("\n"), section, FOOTER);
        if estimate_tokens(&candidate) > max_tokens {
            break;
        }
        kept.push(section.to_owned());
    }
    if kept.len() == 1 {
        return None;
    }
    kept.push(FOOTER.to_owned());
    Some(kept.join("\n"))
}

/// A deliberately conservative token estimate used only for the guidance cap.
pub fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0usize;
    let mut in_word = false;
    let mut word_len = 0usize;
    for character in text.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '-') {
            if !in_word {
                in_word = true;
                word_len = 0;
            }
            word_len += 1;
        } else {
            if in_word {
                tokens += word_len.div_ceil(2);
                in_word = false;
            }
            if !character.is_whitespace() {
                tokens += 1;
            }
        }
    }
    if in_word {
        tokens += word_len.div_ceil(2);
    }
    tokens
}
