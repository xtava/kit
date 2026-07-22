use crate::line::CellRef;
use core::convert::TryInto;
use core::num::NonZeroU8;
use finl_unicode::grapheme_clusters::Graphemes;
use fixedbitset::FixedBitSet;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use wezterm_cell::{Cell, CellAttributes};

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
struct Cluster {
    cell_width: u16,
    attrs: CellAttributes,
}

/// Stores line data as a contiguous string and a series of
/// clusters of attribute data describing attributed ranges
/// within the line
#[cfg_attr(feature = "use_serde", derive(Serialize))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClusteredLine {
    pub text: String,
    #[cfg_attr(feature = "use_serde", serde(serialize_with = "serialize_bitset"))]
    is_double_wide: Option<Box<FixedBitSet>>,
    clusters: Vec<Cluster>,
    /// Length, measured in cells
    len: u32,
    last_cell_width: Option<NonZeroU8>,
}

#[cfg(feature = "use_serde")]
#[derive(Deserialize)]
#[serde(rename = "ClusteredLine")]
struct SerializedClusteredLine {
    text: String,
    is_double_wide: Vec<usize>,
    clusters: Vec<Cluster>,
    len: u32,
    // This is only a cache. Decode the wire value, but never trust it.
    last_cell_width: Option<u8>,
}

/// Serialize the bitset as a vector of the indices of just the 1 bits;
/// the thesis is that most of the cells on a given line are single width.
/// That may not be strictly true for users that heavily use asian scripts,
/// but we'll start with this and see if we need to improve it.
#[cfg(feature = "use_serde")]
fn serialize_bitset<S>(value: &Option<Box<FixedBitSet>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut wide_indices: Vec<usize> = vec![];
    if let Some(bits) = value {
        for idx in bits.ones() {
            wide_indices.push(idx);
        }
    }
    wide_indices.serialize(serializer)
}

#[cfg(feature = "use_serde")]
impl ClusteredLine {
    fn from_serialized(wire: SerializedClusteredLine) -> Result<Self, &'static str> {
        let SerializedClusteredLine {
            text,
            is_double_wide: wide_indices,
            clusters,
            len,
            last_cell_width: _cached_last_cell_width,
        } = wire;

        let len = len as usize;

        let mut prior_wide_index = None;
        for &wide_index in &wide_indices {
            if wide_index >= len {
                return Err("double-wide cell start is outside the line");
            }
            if matches!(prior_wide_index, Some(prior) if prior >= wide_index) {
                return Err("double-wide cell starts must be unique and strictly increasing");
            }
            prior_wide_index = Some(wide_index);
        }

        let mut clustered_width = 0usize;
        for cluster in &clusters {
            if cluster.cell_width == 0 {
                return Err("attribute clusters must have non-zero cell width");
            }
            clustered_width = clustered_width
                .checked_add(cluster.cell_width as usize)
                .ok_or("attribute cluster widths overflow")?;
            if clustered_width > len {
                return Err("attribute clusters overrun the line");
            }
        }
        if clustered_width != len {
            return Err("attribute cluster widths do not cover the line");
        }

        let mut cell_offset = 0usize;
        let mut wide_cursor = 0usize;
        let mut cluster_cursor = 0usize;
        let mut cluster_end = clusters
            .first()
            .map_or(0, |cluster| cluster.cell_width as usize);
        let mut final_width = None;

        for _grapheme in Graphemes::new(&text) {
            let width = match wide_indices.get(wide_cursor).copied() {
                Some(wide_index) if wide_index < cell_offset => {
                    return Err("double-wide cell start is not aligned to a grapheme");
                }
                Some(wide_index) if wide_index == cell_offset => {
                    wide_cursor += 1;
                    2usize
                }
                _ => 1usize,
            };
            let next_offset = cell_offset
                .checked_add(width)
                .ok_or("grapheme cell widths overflow")?;

            if cluster_cursor >= clusters.len() || next_offset > cluster_end {
                return Err("attribute cluster boundary splits a grapheme");
            }

            cell_offset = next_offset;
            final_width = NonZeroU8::new(width as u8);

            if cell_offset == cluster_end {
                cluster_cursor += 1;
                if let Some(cluster) = clusters.get(cluster_cursor) {
                    cluster_end = cluster_end
                        .checked_add(cluster.cell_width as usize)
                        .ok_or("attribute cluster offsets overflow")?;
                }
            }
        }

        if wide_cursor != wide_indices.len() {
            return Err("double-wide cell start does not identify a grapheme");
        }
        if cell_offset != len {
            return Err("grapheme cell widths disagree with the line length");
        }
        if cluster_cursor != clusters.len() {
            return Err("attribute clusters contain cells without graphemes");
        }

        let is_double_wide = if wide_indices.is_empty() {
            None
        } else {
            // Every index and the complete cell layout have been validated. In
            // particular, the bitset length comes from a validated in-range
            // index, never from an unchecked attacker-selected maximum. Match
            // the canonical append representation by ending at the final wide
            // start rather than at the line's cell length.
            let bitset_len = wide_indices
                .last()
                .and_then(|index| index.checked_add(1))
                .ok_or("double-wide cell start overflows bitset length")?;
            let mut bits = FixedBitSet::with_capacity(bitset_len);
            for wide_index in wide_indices {
                bits.set(wide_index, true);
            }
            Some(Box::new(bits))
        };

        Ok(Self {
            text,
            is_double_wide,
            clusters,
            len: len as u32,
            last_cell_width: final_width,
        })
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for ClusteredLine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SerializedClusteredLine::deserialize(deserializer)?;
        Self::from_serialized(wire).map_err(<D::Error as serde::de::Error>::custom)
    }
}

impl ClusteredLine {
    pub(crate) fn retained_heap_size_excluding_image_data(&self) -> usize {
        let cluster_size = self.clusters.iter().fold(
            self.clusters
                .capacity()
                .saturating_mul(core::mem::size_of::<Cluster>()),
            |size, cluster| {
                size.saturating_add(cluster.attrs.retained_heap_size_excluding_image_data())
            },
        );
        let bitset_size = self.is_double_wide.as_ref().map_or(0, |bits| {
            core::mem::size_of::<FixedBitSet>().saturating_add(
                bits.as_slice()
                    .len()
                    .saturating_mul(core::mem::size_of::<u32>()),
            )
        });
        self.text
            .capacity()
            .saturating_add(cluster_size)
            .saturating_add(bitset_size)
    }

    pub fn new() -> Self {
        Self {
            text: String::with_capacity(crate::line::Line::INITIAL_CLUSTER_TEXT_CAPACITY),
            is_double_wide: None,
            clusters: vec![],
            len: 0,
            last_cell_width: None,
        }
    }

    pub fn to_cell_vec(&self) -> Vec<Cell> {
        let mut cells = Vec::with_capacity(self.len as usize);

        for c in self.iter() {
            cells.push(c.as_cell());
            for _ in 1..c.width() {
                cells.push(Cell::blank_with_attrs(c.attrs().clone()));
            }
        }

        cells
    }

    pub fn from_cell_vec<'a>(hint: usize, iter: impl Iterator<Item = CellRef<'a>>) -> Self {
        let mut last_cluster: Option<Cluster> = None;
        let mut is_double_wide = FixedBitSet::with_capacity(hint);
        let mut text = String::new();
        let mut clusters = vec![];
        let mut any_double = false;
        let mut len = 0;
        let mut last_cell_width = None;

        for cell in iter {
            len += cell.width();
            last_cell_width = NonZeroU8::new(1);

            if cell.width() > 1 {
                any_double = true;
                is_double_wide.set(cell.cell_index(), true);
            }

            text.push_str(cell.str());

            last_cluster = match last_cluster.take() {
                None => Some(Cluster {
                    cell_width: cell.width() as u16,
                    attrs: cell.attrs().clone(),
                }),
                Some(cluster) if cluster.attrs != *cell.attrs() => {
                    clusters.push(cluster);
                    Some(Cluster {
                        cell_width: cell.width() as u16,
                        attrs: cell.attrs().clone(),
                    })
                }
                Some(mut cluster) => {
                    cluster.cell_width += cell.width() as u16;
                    Some(cluster)
                }
            };
        }

        if let Some(cluster) = last_cluster.take() {
            clusters.push(cluster);
        }

        Self {
            text,
            is_double_wide: if any_double {
                Some(Box::new(is_double_wide))
            } else {
                None
            },
            clusters,
            len: len.try_into().unwrap(),
            last_cell_width,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    fn is_double_wide(&self, cell_index: usize) -> bool {
        match &self.is_double_wide {
            Some(bitset) => bitset.contains(cell_index),
            None => false,
        }
    }

    pub fn iter(&self) -> ClusterLineCellIter<'_> {
        let mut clusters = self.clusters.iter();
        let cluster = clusters.next();
        ClusterLineCellIter {
            graphemes: Graphemes::new(&self.text),
            clusters,
            cluster,
            idx: 0,
            cluster_total: 0,
            line: self,
        }
    }

    pub fn append_grapheme(&mut self, text: &str, cell_width: usize, attrs: CellAttributes) {
        let cell_width = cell_width as u16;
        let new_cluster = match self.clusters.last() {
            Some(cluster) => {
                if cluster.attrs != attrs {
                    true
                } else {
                    // If we overflow the max length of a run,
                    // then we need a new cluster
                    let (_, did_overflow) = cluster.cell_width.overflowing_add(cell_width);
                    did_overflow
                }
            }
            None => true,
        };
        let new_cell_index = self.len as usize;
        if new_cluster {
            self.clusters.push(Cluster { attrs, cell_width });
        } else if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width += cell_width;
        }
        self.text.push_str(text);

        if cell_width > 1 {
            let bitset = match self.is_double_wide.take() {
                Some(mut bitset) => {
                    bitset.grow(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    bitset
                }
                None => {
                    let mut bitset = FixedBitSet::with_capacity(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    Box::new(bitset)
                }
            };
            self.is_double_wide.replace(bitset);
        }
        self.last_cell_width = NonZeroU8::new(cell_width as u8);
        self.len += cell_width as u32;
    }

    pub fn append(&mut self, cell: Cell) {
        let cell_width = cell.width() as u16;
        let new_cluster = match self.clusters.last() {
            Some(cluster) => {
                if cluster.attrs != *cell.attrs() {
                    true
                } else {
                    // If we overflow the max length of a run,
                    // then we need a new cluster
                    let (_, did_overflow) = cluster.cell_width.overflowing_add(cell_width);
                    did_overflow
                }
            }
            None => true,
        };
        let new_cell_index = self.len as usize;
        if new_cluster {
            self.clusters.push(Cluster {
                attrs: (*cell.attrs()).clone(),
                cell_width,
            });
        } else if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width += cell_width;
        }
        self.text.push_str(cell.str());

        if cell_width > 1 {
            let bitset = match self.is_double_wide.take() {
                Some(mut bitset) => {
                    bitset.grow(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    bitset
                }
                None => {
                    let mut bitset = FixedBitSet::with_capacity(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    Box::new(bitset)
                }
            };
            self.is_double_wide.replace(bitset);
        }
        self.last_cell_width = NonZeroU8::new(cell_width as u8);
        self.len += cell_width as u32;
    }

    pub fn prune_trailing_blanks(&mut self) -> bool {
        let num_spaces = self.text.chars().rev().take_while(|&c| c == ' ').count();
        if num_spaces == 0 {
            return false;
        }

        let blank = CellAttributes::blank();
        let mut pruned = false;
        for _ in 0..num_spaces {
            let mut need_pop = false;
            if let Some(cluster) = self.clusters.last_mut() {
                if cluster.attrs != blank {
                    break;
                }
                cluster.cell_width -= 1;
                self.text.pop();
                self.len -= 1;
                self.last_cell_width.take();
                pruned = true;
                if cluster.cell_width == 0 {
                    need_pop = true;
                }
            }
            if need_pop {
                self.clusters.pop();
            }
        }

        pruned
    }

    fn compute_last_cell_width(&mut self) -> Option<NonZeroU8> {
        if self.last_cell_width.is_none() {
            if let Some(last_cell) = self.iter().last() {
                self.last_cell_width = NonZeroU8::new(last_cell.width() as u8);
            }
        }
        self.last_cell_width
    }

    pub fn set_last_cell_was_wrapped(&mut self, wrapped: bool) {
        if let Some(width) = self.compute_last_cell_width() {
            let width = width.get() as u16;
            if let Some(last_cluster) = self.clusters.last_mut() {
                let mut attrs = last_cluster.attrs.clone();
                attrs.set_wrapped(wrapped);

                if last_cluster.cell_width == width {
                    // Re-purpose final cluster
                    last_cluster.attrs = attrs;
                } else {
                    last_cluster.cell_width -= width;
                    self.clusters.push(Cluster {
                        cell_width: width,
                        attrs,
                    });
                }
            }
        }
    }
}

pub(crate) struct ClusterLineCellIter<'a> {
    graphemes: Graphemes<'a>,
    clusters: core::slice::Iter<'a, Cluster>,
    cluster: Option<&'a Cluster>,
    idx: usize,
    cluster_total: usize,
    line: &'a ClusteredLine,
}

impl<'a> Iterator for ClusterLineCellIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<CellRef<'a>> {
        let text = self.graphemes.next()?;

        let cell_index = self.idx;
        let width = if self.line.is_double_wide(cell_index) {
            2
        } else {
            1
        };
        self.idx += width;
        self.cluster_total += width;
        let attrs = &self.cluster.as_ref()?.attrs;

        if self.cluster_total >= self.cluster.as_ref()?.cell_width as usize {
            self.cluster = self.clusters.next();
            self.cluster_total = 0;
        }

        Some(CellRef::ClusterRef {
            cell_index,
            width,
            text,
            attrs,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[cfg(feature = "use_serde")]
    fn cluster(cell_width: u16) -> Cluster {
        Cluster {
            cell_width,
            attrs: CellAttributes::blank(),
        }
    }

    #[cfg(feature = "use_serde")]
    fn wire(
        text: &str,
        wide_indices: Vec<usize>,
        cluster_widths: &[u16],
        len: u32,
        cached_last_width: Option<u8>,
    ) -> SerializedClusteredLine {
        SerializedClusteredLine {
            text: String::from(text),
            is_double_wide: wide_indices,
            clusters: cluster_widths.iter().copied().map(cluster).collect(),
            len,
            last_cell_width: cached_last_width,
        }
    }

    #[cfg(feature = "use_serde")]
    fn assert_invalid(wire: SerializedClusteredLine, expected: &str) {
        let error = ClusteredLine::from_serialized(wire).unwrap_err();
        assert_eq!(error, expected);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn memory_usage() {
        assert_eq!(core::mem::size_of::<ClusteredLine>(), 64);
        assert_eq!(core::mem::size_of::<String>(), 24);
        assert_eq!(core::mem::size_of::<Vec<Cluster>>(), 24);
        assert_eq!(core::mem::size_of::<Option<Box<FixedBitSet>>>(), 8);
        assert_eq!(core::mem::size_of::<Option<NonZeroU8>>(), 1);
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_valid_roundtrip_rebuilds_canonical_line() {
        let attrs = CellAttributes::blank();
        let mut original = ClusteredLine::new();
        original.append_grapheme("a", 1, attrs.clone());
        original.append_grapheme("界", 2, attrs);

        let wide_indices = original
            .is_double_wide
            .as_ref()
            .map(|bits| bits.ones().collect())
            .unwrap_or_default();
        let decoded = ClusteredLine::from_serialized(SerializedClusteredLine {
            text: original.text.clone(),
            is_double_wide: wide_indices,
            clusters: original.clusters.clone(),
            len: original.len,
            last_cell_width: original.last_cell_width.map(NonZeroU8::get),
        })
        .unwrap();

        assert_eq!(decoded, original);
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_accepts_empty_and_wide_boundary_layouts() {
        let empty = ClusteredLine::from_serialized(wire("", vec![], &[], 0, None)).unwrap();
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.last_cell_width, None);

        let boundary =
            ClusteredLine::from_serialized(wire("a界", vec![1], &[1, 2], 3, Some(2))).unwrap();
        assert_eq!(boundary.len(), 3);
        assert!(boundary.is_double_wide(1));
        assert_eq!(boundary.last_cell_width, NonZeroU8::new(2));
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_rejects_zero_and_overrunning_clusters() {
        assert_invalid(
            wire("", vec![], &[0], 0, None),
            "attribute clusters must have non-zero cell width",
        );
        assert_invalid(
            wire("a", vec![], &[2], 1, Some(1)),
            "attribute clusters overrun the line",
        );
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_rejects_cluster_boundary_inside_wide_grapheme() {
        assert_invalid(
            wire("界", vec![0], &[1, 1], 2, Some(2)),
            "attribute cluster boundary splits a grapheme",
        );
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_rejects_duplicate_or_orphan_wide_starts() {
        assert_invalid(
            wire("界", vec![0, 0], &[2], 2, Some(2)),
            "double-wide cell starts must be unique and strictly increasing",
        );
        assert_invalid(
            wire("ab", vec![0, 1], &[3], 3, Some(1)),
            "double-wide cell start is not aligned to a grapheme",
        );
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_rejects_out_of_range_wide_start_without_allocating_for_it() {
        assert_invalid(
            wire("a", vec![usize::MAX], &[1], 1, Some(1)),
            "double-wide cell start is outside the line",
        );
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_rejects_grapheme_and_cell_length_disagreement() {
        assert_invalid(
            wire("ab", vec![], &[3], 3, Some(1)),
            "grapheme cell widths disagree with the line length",
        );
        assert_invalid(
            wire("", vec![], &[1], 1, None),
            "grapheme cell widths disagree with the line length",
        );
    }

    #[test]
    #[cfg(feature = "use_serde")]
    fn serde_wire_ignores_hostile_cached_final_width() {
        let zero = ClusteredLine::from_serialized(wire("界", vec![0], &[2], 2, Some(0))).unwrap();
        let unrelated =
            ClusteredLine::from_serialized(wire("界", vec![0], &[2], 2, Some(u8::MAX))).unwrap();

        assert_eq!(zero.last_cell_width, NonZeroU8::new(2));
        assert_eq!(unrelated.last_cell_width, NonZeroU8::new(2));
    }
}
