//! Optional explicit-receiver enrichment for tag-based OCaml extraction.

use super::{
    ExtractError, ExtractedFile, ExtractionBuilder, MAX_AST_DEPTH, Node, SourceLanguage,
    SourceSnapshot, explicit_receivers,
};

pub(crate) fn enrich(
    snapshot: &SourceSnapshot,
    input: (Node<'_>, ExtractedFile),
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ExtractedFile, ExtractError> {
    let (root, mut file) = input;
    if snapshot.language() != SourceLanguage::Ocaml {
        return Ok(file);
    }
    let mut builder = ExtractionBuilder::new(snapshot, MAX_AST_DEPTH, cancelled)?;
    if builder
        .context
        .budget
        .reserve_working_bytes(file.modeled_retained_bytes())
        .is_err()
    {
        return Ok(file);
    }
    builder.facts.symbols = std::mem::take(&mut file.symbols);
    builder.facts.references = std::mem::take(&mut file.references);
    let evidence = explicit_receivers::enrich(&mut builder, root)
        .and_then(|()| explicit_receivers::finish(&mut builder));
    file.symbols = builder.facts.symbols;
    file.references = builder.facts.references;
    match evidence {
        Ok(evidence) => file.receiver_evidence = evidence,
        Err(ExtractError::OutputLimit) => {}
        Err(error) => return Err(error),
    }
    if file.modeled_retained_bytes() > builder.context.budget.output_limit() {
        file.receiver_evidence = None;
    }
    Ok(file)
}
