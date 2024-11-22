use cairo_lang_stable_token::{StableSpan, StableToken, ToStableTokenStream};

use super::SyntaxNode;
use super::db::SyntaxGroup;

pub struct SyntaxNodeWithDb<'a, Db: SyntaxGroup> {
    node: &'a SyntaxNode,
    db: &'a Db,
}

impl<'a, Db: SyntaxGroup> SyntaxNodeWithDb<'a, Db> {
    pub fn new(node: &'a SyntaxNode, db: &'a Db) -> Self {
        Self { node, db }
    }
}

impl<'a, Db: SyntaxGroup> ToStableTokenStream for SyntaxNodeWithDb<'a, Db> {
    type Iter = SyntaxNodeWithDbIterator<'a, Db>;

    fn to_stable_token_stream(&self) -> Self::Iter {
        // The lifetime of the iterator should extend 'a because it derives from both node and db
        SyntaxNodeWithDbIterator::new(Box::new(self.node.tokens(self.db)), self.db)
    }
}

pub struct SyntaxNodeWithDbIterator<'a, Db: SyntaxGroup> {
    inner: Box<dyn Iterator<Item = SyntaxNode> + 'a>,
    db: &'a Db,
}

impl<'a, Db: SyntaxGroup> SyntaxNodeWithDbIterator<'a, Db> {
    pub fn new(inner: Box<dyn Iterator<Item = SyntaxNode> + 'a>, db: &'a Db) -> Self {
        Self { inner, db }
    }
}

impl<Db: SyntaxGroup> Iterator for SyntaxNodeWithDbIterator<'_, Db> {
    type Item = StableToken;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|node| {
            let span = node.span(self.db).to_str_range();
            StableToken {
                content: node.get_text(self.db),
                span: Some(StableSpan { start: span.start, end: span.end }),
            }
        })
    }
}
