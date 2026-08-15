use crate::model::{logit, sigmoid, GlobalScoringContext};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default)]
pub struct Evidence {
    pub lexical: f32,
    pub vector: f32,
    pub has_lexical: bool,
    pub has_vector: bool,
}

impl Evidence {
    #[must_use]
    pub fn fused_logit(&self, ctx: &GlobalScoringContext) -> f32 {
        logit(ctx.base_rate)
            + if self.has_lexical { self.lexical * ctx.lexical_weight } else { 0.0 }
            + if self.has_vector { self.vector * ctx.vector_weight } else { 0.0 }
    }
    #[must_use]
    pub fn posterior(&self, ctx: &GlobalScoringContext) -> f32 { sigmoid(self.fused_logit(ctx)) }
}

pub fn merge_evidence(lexical: impl IntoIterator<Item=(u32,f32)>, vector: impl IntoIterator<Item=(u32,f32)>) -> BTreeMap<u32,Evidence> {
    let mut out = BTreeMap::new();
    for (id, e) in lexical { let x = out.entry(id).or_insert_with(Evidence::default); x.lexical += e; x.has_lexical = true; }
    for (id, e) in vector { let x = out.entry(id).or_insert_with(Evidence::default); x.vector += e; x.has_vector = true; }
    out
}
