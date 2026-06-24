//! Aggregate repository handle.

use crate::{GameRepo, RatingRepo, SeekRepo, SessionRepo, UserRepo};

/// A single handle that exposes all repository traits.
///
/// Application code (request handlers, background tasks, etc.) should depend
/// on this trait rather than holding individual repos — it provides a single
/// injection point that is easy to mock, wrap with middleware, or swap out
/// entirely.
///
/// ## Object-safety design
///
/// Generic-associated-type (GAT) accessors would force the trait to be
/// non-object-safe. Instead, each accessor returns a `&dyn Trait` reference
/// borrowed from `self`. This is the simplest object-safe design and works
/// well because:
///
/// - All concrete implementations hold the sub-repos behind an `Arc` or inline,
///   so the borrow is cheap.
/// - Callers can hold a `&dyn Repositories` or an `Arc<dyn Repositories>`
///   with zero runtime overhead for the dispatch.
///
/// ## Example (pseudo-code)
///
/// ```rust,ignore
/// async fn handle_seek(repos: &dyn Repositories, seek: &Seek) {
///     repos.seeks().create(seek).await?;
///     let open = repos.seeks().list_open().await?;
///     // ...
/// }
/// ```
pub trait Repositories: Send + Sync {
    /// Returns the user repository.
    fn users(&self) -> &dyn UserRepo;

    /// Returns the game repository.
    fn games(&self) -> &dyn GameRepo;

    /// Returns the seek repository.
    fn seeks(&self) -> &dyn SeekRepo;

    /// Returns the session / nonce repository.
    fn sessions(&self) -> &dyn SessionRepo;

    /// Returns the rating repository.
    fn ratings(&self) -> &dyn RatingRepo;
}
