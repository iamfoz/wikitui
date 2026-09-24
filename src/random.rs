//! PRD FR-SR-5's "random good article" policy: batch [`GOOD_BATCH`] titles
//! from `list=random` in one call, then one batched `prop=pageassessments`
//! call for the whole batch (§6.2 rule 10 / NF-NET-5 — never a per-title
//! fanout), and take the *first* title whose assessment is ≥ GA. Kept
//! separate from `api.rs` (which only knows wire shapes) so the decision
//! itself — first hit wins, else fall back to the batch's first title
//! rather than erroring — is unit-testable against a plain `HashMap`, no
//! network involved. The plain (non-"good") `gr`/`:random` binding needs
//! none of this: it's `WikiClient::random_titles(lang, 1)` directly.

use crate::api::{QualityClass, WikiClient};
use anyhow::Result;
use std::collections::HashMap;

/// PRD FR-SR-5's documented batch size ("batch 10 random titles + one
/// assessments query").
pub const GOOD_BATCH: u32 = 10;

/// What `:random good` (and the documented `gr`-adjacent "random good
/// article" variant) resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RandomGood {
    /// A title in the batch is assessed ≥ GA — open it.
    Found(String),
    /// Nothing in the batch reached GA. §7's graceful-degradation posture:
    /// open a random title from the same batch anyway rather than erroring
    /// or leaving the reader with nothing; the caller surfaces the
    /// "no good article found" notice alongside it.
    Fallback(String),
    /// The batch itself came back empty (the endpoint returned nothing to
    /// pick from at all).
    Empty,
}

/// The pure decision (no network, no randomness of its own): the first
/// batch title whose assessment is ≥ GA wins — "first", not "best in the
/// batch", matching FR-SR-5's literal "pick the first GA+" — else the
/// batch's own first title as the documented fallback, else `Empty` when
/// the batch itself was empty.
pub fn pick_first_good(
    titles: &[String],
    assessments: &HashMap<String, QualityClass>,
) -> RandomGood {
    for title in titles {
        if assessments
            .get(title)
            .is_some_and(|class| class.is_good_or_better())
        {
            return RandomGood::Found(title.clone());
        }
    }
    match titles.first() {
        Some(title) => RandomGood::Fallback(title.clone()),
        None => RandomGood::Empty,
    }
}

/// The composed network call (PRD FR-SR-5 / Appendix A): one batched
/// `list=random` call, then (only if it returned anything) one batched
/// `prop=pageassessments` call, then [`pick_first_good`].
pub async fn pick_random_good(client: &WikiClient, lang: &str) -> Result<RandomGood> {
    let titles = client.random_titles(lang, GOOD_BATCH).await?;
    if titles.is_empty() {
        return Ok(RandomGood::Empty);
    }
    let assessments = client.page_assessments(lang, &titles).await?;
    Ok(pick_first_good(&titles, &assessments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("Article {i}")).collect()
    }

    /// PRD FR-SR-5: a batch of 10 with one GA+ title (not the first one
    /// drawn) still resolves to that title — "first GA+ wins" is evaluated
    /// in batch order, not by scanning for the best.
    #[test]
    fn batch_of_ten_resolves_to_the_first_ga_or_better_title() {
        let batch = titles(10);
        let mut assessments = HashMap::new();
        assessments.insert(batch[3].clone(), QualityClass::Ga);
        assessments.insert(batch[7].clone(), QualityClass::Fa);
        assessments.insert(batch[1].clone(), QualityClass::B); // below the bar

        assert_eq!(
            pick_first_good(&batch, &assessments),
            RandomGood::Found(batch[3].clone()),
            "the earlier-drawn GA+ title wins over the later FA"
        );
    }

    /// PRD FR-SR-5 / §7: when nothing in the batch reaches GA, fall back to
    /// the batch's own first title rather than erroring.
    #[test]
    fn batch_with_no_good_article_falls_back_to_the_first_title() {
        let batch = titles(10);
        let mut assessments = HashMap::new();
        assessments.insert(batch[2].clone(), QualityClass::Start);
        assessments.insert(batch[5].clone(), QualityClass::Stub);
        // The rest are unassessed (absent from the map entirely).

        assert_eq!(
            pick_first_good(&batch, &assessments),
            RandomGood::Fallback(batch[0].clone())
        );
    }

    /// An empty batch (the `list=random` call itself returned nothing) must
    /// not panic or synthesize a fallback title out of thin air.
    #[test]
    fn empty_batch_is_empty() {
        assert_eq!(pick_first_good(&[], &HashMap::new()), RandomGood::Empty);
    }

    /// A title with no entry in the assessments map at all (as opposed to
    /// one with a recognized-but-below-GA class) is treated identically to
    /// "not good enough" — never a false positive.
    #[test]
    fn a_title_absent_from_the_assessments_map_does_not_count_as_good() {
        let batch = titles(3);
        let assessments = HashMap::new(); // nothing assessed
        assert_eq!(
            pick_first_good(&batch, &assessments),
            RandomGood::Fallback(batch[0].clone())
        );
    }
}
