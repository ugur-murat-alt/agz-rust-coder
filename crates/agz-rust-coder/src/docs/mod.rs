//! Exact-version documentation resolution and its bounded cache helpers.

pub mod cache;
pub mod html;
pub mod resolver;

pub use cache::{
    CacheIdentity, CachedPage, DOCS_COMPLETE_MARKER, DocsCache, DocsCacheError, GeneratedPage,
};
pub use html::{
    DOCS_MAX_HTML_BYTES, DOCS_MAX_OUTPUT, decode_html_entities, extract_rustdoc_text,
    is_safe_page_path, package_folder_names, page_candidates, strip_rustdoc_html,
    symbol_page_candidates,
};
pub use resolver::{
    CargoDocGenerator, CargoLockCandidate, CargoLockSource, DOCS_FETCH_TIMEOUT, DOCS_USER_AGENT,
    DocsFallback, DocsInput, DocsOptions, DocsProvider, DocsResolver, DocsResult, DocsStatus,
    DocsUrlError, LocalDocGenerator, LocalDocRequest, NetworkClient, NetworkRequest,
    NetworkResponse, ReqwestNetworkClient, ResolverError, UnavailableLocalGenerator,
    UnavailableNetworkClient, build_docs_rs_url, docs_rs_url, is_crates_io_registry,
    is_valid_docs_rs_url, parse_cargo_lock_dependency, resolve_docs, resolve_docs_default,
};
