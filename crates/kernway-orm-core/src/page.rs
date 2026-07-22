#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items:       Vec<T>,
    pub total:       u64,
    pub page:        u64,
    pub size:        u64,
    pub total_pages: u64,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: u64, page: u64, size: u64) -> Self {
        let total_pages = if size == 0 {
            0
        } else {
            total.div_ceil(size)
        };
        Self {
            items,
            total,
            page,
            size,
            total_pages,
        }
    }

    pub fn empty() -> Self {
        Self::new(vec![], 0, 0, 20)
    }

    pub fn is_last(&self) -> bool {
        self.page + 1 >= self.total_pages
    }

    pub fn has_next(&self) -> bool {
        !self.is_last()
    }
}

#[cfg(test)]
mod tests {
    use super::Page;

    #[test]
    fn page_total_pages_calculation() {
        let page = Page::new(vec![1, 2], 5, 0, 2);
        assert_eq!(page.total_pages, 3);
    }

    #[test]
    fn page_is_last() {
        let page = Page::new(vec![5], 5, 2, 2);
        assert!(page.is_last());
    }

    #[test]
    fn page_has_next() {
        let page = Page::new(vec![1, 2], 5, 0, 2);
        assert!(page.has_next());
    }

    #[test]
    fn page_empty() {
        let page: Page<u64> = Page::empty();
        assert!(page.items.is_empty());
        assert_eq!(page.total, 0);
        assert_eq!(page.page, 0);
        assert_eq!(page.size, 20);
        assert_eq!(page.total_pages, 0);
    }
}
