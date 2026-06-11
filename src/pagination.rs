use crate::models::{CategoryHit, FileHit};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Number of buttons shown on one paginated Telegram result page.
pub const BUTTON_PAGE_SIZE: usize = 20;

const MAX_PAGINATED_LISTS: usize = 256;

static FILE_LISTS: Lazy<RwLock<HashMap<String, Vec<FileHit>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static CATEGORY_LISTS: Lazy<RwLock<HashMap<String, Vec<CategoryHit>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// One page of a stored paginated result list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaginatedPage<T> {
    /// Items that should be rendered on the current page.
    pub items: Vec<T>,
    /// Zero-based page index.
    pub page_index: usize,
    /// Total number of pages in the stored list.
    pub total_pages: usize,
    /// Total number of items in the stored list.
    pub total_items: usize,
}

/// Stores a file result list and returns a short callback token for it.
pub async fn store_file_list(files: &[FileHit]) -> String {
    let token = file_list_token(files);
    insert_bounded(&FILE_LISTS, token.clone(), files.to_vec()).await;
    token
}

/// Stores a category result list and returns a short callback token for it.
pub async fn store_category_list(categories: &[CategoryHit]) -> String {
    let token = category_list_token(categories);
    insert_bounded(&CATEGORY_LISTS, token.clone(), categories.to_vec()).await;
    token
}

/// Returns one stored file page, or `None` when the token expired.
pub async fn file_page(token: &str, page_index: usize) -> Option<PaginatedPage<FileHit>> {
    let lists = FILE_LISTS.read().await;
    page_from_items(lists.get(token)?, page_index)
}

/// Returns one stored category page, or `None` when the token expired.
pub async fn category_page(token: &str, page_index: usize) -> Option<PaginatedPage<CategoryHit>> {
    let lists = CATEGORY_LISTS.read().await;
    page_from_items(lists.get(token)?, page_index)
}

/// Returns the number of pages required for a result list.
pub fn page_count(total_items: usize) -> usize {
    total_items.div_ceil(BUTTON_PAGE_SIZE).max(1)
}

/// Computes a callback token for a list of files.
pub fn file_list_token(files: &[FileHit]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"files");
    for file in files {
        hasher.update(file.page_id.to_le_bytes());
        hasher.update(file.title.as_bytes());
        hasher.update([0]);
    }
    short_token(hasher)
}

/// Computes a callback token for a list of categories.
pub fn category_list_token(categories: &[CategoryHit]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"categories");
    for category in categories {
        hasher.update(category.page_id.to_le_bytes());
        hasher.update(category.title.as_bytes());
        hasher.update([0]);
    }
    short_token(hasher)
}

/// Inserts a paginated list while bounding warm-container RAM use.
async fn insert_bounded<T: Clone>(
    lists: &RwLock<HashMap<String, Vec<T>>>,
    token: String,
    items: Vec<T>,
) {
    let mut lists = lists.write().await;
    if lists.len() >= MAX_PAGINATED_LISTS
        && !lists.contains_key(&token)
        && let Some(key) = lists.keys().next().cloned()
    {
        lists.remove(&key);
    }
    lists.insert(token, items);
}

/// Returns one page of an in-memory list.
fn page_from_items<T: Clone>(items: &[T], page_index: usize) -> Option<PaginatedPage<T>> {
    if items.is_empty() {
        return None;
    }
    let total_items = items.len();
    let total_pages = page_count(total_items);
    let page_index = page_index.min(total_pages - 1);
    let start = page_index * BUTTON_PAGE_SIZE;
    let end = (start + BUTTON_PAGE_SIZE).min(total_items);
    Some(PaginatedPage {
        items: items[start..end].to_vec(),
        page_index,
        total_pages,
        total_items,
    })
}

/// Converts a hash into a compact Telegram callback-safe token.
fn short_token(hasher: Sha256) -> String {
    hex::encode(hasher.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::{BUTTON_PAGE_SIZE, file_list_token, page_count, page_from_items};
    use crate::models::FileHit;

    #[test]
    fn counts_button_pages() {
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(1), 1);
        assert_eq!(page_count(BUTTON_PAGE_SIZE), 1);
        assert_eq!(page_count(BUTTON_PAGE_SIZE + 1), 2);
    }

    #[test]
    fn returns_requested_page_items() {
        let files = (0..45)
            .map(|page_id| FileHit {
                page_id,
                title: format!("File:{page_id}.jpg"),
                ..FileHit::default()
            })
            .collect::<Vec<_>>();

        let page = page_from_items(&files, 2).unwrap();

        assert_eq!(page.page_index, 2);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.items.len(), 5);
        assert_eq!(page.items[0].page_id, 40);
    }

    #[test]
    fn file_tokens_change_with_order() {
        let first = vec![
            FileHit {
                page_id: 1,
                title: "File:A.jpg".into(),
                ..FileHit::default()
            },
            FileHit {
                page_id: 2,
                title: "File:B.jpg".into(),
                ..FileHit::default()
            },
        ];
        let mut second = first.clone();
        second.reverse();

        assert_ne!(file_list_token(&first), file_list_token(&second));
    }
}
