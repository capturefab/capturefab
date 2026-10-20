//! Which pending item a reviewer sees next. Every candidate keeps its
//! distance to the nearest reviewed item, kept item and discarded item; a
//! decision only ever lowers those, so it updates them in one pass and the
//! session persists only the ones that changed.
use crate::autocapture::{Descriptor, Encoder};

/// How a session orders its pending items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rank {
    /// The item least like everything reviewed so far comes first.
    Novelty,
    /// The item most like what was kept, and least like what was discarded.
    Similar,
}

impl Rank {
    pub fn name(self) -> &'static str {
        match self {
            Self::Novelty => "novelty",
            Self::Similar => "similar",
        }
    }
    pub(crate) fn parse(name: &str) -> anyhow::Result<Self> {
        match name {
            "novelty" => Ok(Self::Novelty),
            "similar" => Ok(Self::Similar),
            other => anyhow::bail!("unsupported folder review rank {other}"),
        }
    }
}

/// A pending item with its nearest distances. `None` means nothing of that
/// kind exists yet.
pub(crate) struct Candidate {
    pub id: u64,
    pub feature: Descriptor,
    pub near_reviewed: Option<f64>,
    pub near_kept: Option<f64>,
    pub near_discarded: Option<f64>,
    /// Changed since it was loaded or last persisted.
    pub dirty: bool,
}

/// Lowest score that counts as interesting at `sensitivity`, as auto capture
/// suggests a frame.
pub(crate) fn threshold(sensitivity: f64) -> f64 {
    0.90 - 0.35 * sensitivity
}

pub(crate) struct Ranker {
    pub candidates: Vec<Candidate>,
    /// Mean of every candidate. Before anything is reviewed, novelty is the
    /// distance from it, so the folder's most atypical item comes first.
    centroid: Option<Descriptor>,
}

impl Ranker {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        let mut ranker = Self {
            candidates,
            centroid: None,
        };
        ranker.recenter();
        ranker
    }

    fn recenter(&mut self) {
        let Some(first) = self.candidates.first() else {
            self.centroid = None;
            return;
        };
        let mut sum = vec![0.0_f64; first.feature.len()];
        for candidate in &self.candidates {
            for (total, value) in sum.iter_mut().zip(&candidate.feature) {
                *total += f64::from(*value);
            }
        }
        let n = self.candidates.len() as f64;
        self.centroid = Some(sum.into_iter().map(|total| (total / n) as f32).collect());
    }

    pub fn add(&mut self, candidates: impl IntoIterator<Item = Candidate>) {
        self.candidates.extend(candidates);
        self.recenter();
    }

    /// Novelty distance: from the nearest reviewed item, or from the
    /// centroid while nothing has been reviewed.
    fn novelty_distance(&self, encoder: &Encoder, candidate: &Candidate) -> f64 {
        match (candidate.near_reviewed, &self.centroid) {
            (Some(distance), _) => distance,
            (None, Some(centroid)) => encoder.distance(centroid, &candidate.feature),
            (None, None) => 0.0,
        }
    }

    /// A candidate's score in 0..=1 and why.
    pub fn score(&self, encoder: &Encoder, rank: Rank, candidate: &Candidate) -> (f64, String) {
        let metric = encoder.metric();
        let distance = self.novelty_distance(encoder, candidate);
        let novelty = (distance / metric.novelty).clamp(0.0, 1.0);
        let against = if candidate.near_reviewed.is_some() {
            "everything reviewed"
        } else {
            "the folder's typical item"
        };
        let unlike = candidate.near_discarded.map(|d| encoder.similarity(d));
        match (rank, candidate.near_kept) {
            (Rank::Similar, Some(kept)) => {
                // Auto capture's preference: distance-ratio against discards,
                // with novelty promoting an unusual variant of a kept item.
                let affinity = encoder.similarity(kept);
                let preference = match candidate.near_discarded {
                    Some(discarded) if kept + discarded > f64::EPSILON => {
                        affinity * discarded / (kept + discarded)
                    }
                    Some(_) => affinity * 0.5,
                    None => affinity,
                };
                let score = (preference * (0.9 + 0.1 * novelty)).clamp(0.0, 1.0);
                (
                    score,
                    format!(
                        "{:.0}% match to kept items; {:.0}% novelty against {against}{}",
                        affinity * 100.0,
                        novelty * 100.0,
                        if unlike.is_some() {
                            "; adjusted by discarded items"
                        } else {
                            ""
                        }
                    ),
                )
            }
            (rank, _) => {
                let penalty = unlike.unwrap_or(0.0);
                let score = (novelty * (1.0 - penalty)).clamp(0.0, 1.0);
                let mut reason = format!("{:.0}% novelty against {against}", novelty * 100.0);
                if penalty >= 0.005 {
                    reason.push_str(&format!("; {:.0}% like a discarded item", penalty * 100.0));
                }
                if rank == Rank::Similar {
                    reason.push_str("; nothing kept yet, so ranked by novelty");
                }
                (score, reason)
            }
        }
    }

    /// The best candidate, with its score and reason. Below the threshold it
    /// is returned only when `all` asks for every pending item.
    pub fn best(
        &self,
        encoder: &Encoder,
        rank: Rank,
        sensitivity: f64,
        all: bool,
    ) -> Option<(usize, f64, String)> {
        let (index, score) = self
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| (index, self.score(encoder, rank, candidate).0))
            // Ties go to the earlier item, which is the scan order.
            .fold(None::<(usize, f64)>, |best, (index, score)| match best {
                Some((_, top)) if top >= score => best,
                _ => Some((index, score)),
            })?;
        if !all && score < threshold(sensitivity) {
            return None;
        }
        let (_, reason) = self.score(encoder, rank, &self.candidates[index]);
        Some((index, score, reason))
    }

    /// How many candidates are interesting now.
    pub fn interesting(&self, encoder: &Encoder, rank: Rank, sensitivity: f64) -> usize {
        let threshold = threshold(sensitivity);
        self.candidates
            .iter()
            .filter(|candidate| self.score(encoder, rank, candidate).0 >= threshold)
            .count()
    }

    /// Learn from a decision on `feature`: lower every candidate's nearest
    /// distances, and take the ones now within duplicate distance out of the
    /// queue. Returns the identifiers taken out.
    pub fn decided(&mut self, encoder: &Encoder, feature: &Descriptor, keep: bool) -> Vec<u64> {
        let duplicate = encoder.metric().duplicate;
        let mut duplicates = Vec::new();
        self.candidates.retain_mut(|candidate| {
            let distance = encoder.distance(feature, &candidate.feature);
            if distance < duplicate {
                duplicates.push(candidate.id);
                return false;
            }
            let lower = |slot: &mut Option<f64>, dirty: &mut bool| {
                if slot.is_none_or(|nearest| distance < nearest) {
                    *slot = Some(distance);
                    *dirty = true;
                }
            };
            lower(&mut candidate.near_reviewed, &mut candidate.dirty);
            if keep {
                lower(&mut candidate.near_kept, &mut candidate.dirty);
            } else {
                lower(&mut candidate.near_discarded, &mut candidate.dirty);
            }
            true
        });
        duplicates
    }

    pub fn remove(&mut self, id: u64) -> Option<Candidate> {
        let index = self.candidates.iter().position(|c| c.id == id)?;
        Some(self.candidates.remove(index))
    }
}
