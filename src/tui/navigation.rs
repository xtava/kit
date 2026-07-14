use ratatui::layout::Rect;

/// A direction between interactive regions in the rendered terminal layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// One interactive region in a frame-local navigation map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NavigationRegion<Id> {
    pub id: Id,
    pub area: Rect,
}

impl<Id> NavigationRegion<Id> {
    pub const fn new(id: Id, area: Rect) -> Self {
        Self { id, area }
    }
}

/// Resolves keyboard and mouse movement across the interactive regions in one frame.
pub struct NavigationMap<Id> {
    regions: Vec<NavigationRegion<Id>>,
}

impl<Id: Copy + Eq> NavigationMap<Id> {
    pub fn new(regions: impl IntoIterator<Item = NavigationRegion<Id>>) -> Self {
        Self {
            regions: regions
                .into_iter()
                .filter(|region| region.area.width > 0 && region.area.height > 0)
                .collect(),
        }
    }

    pub fn neighbor(&self, current: Id, direction: Direction) -> Option<Id> {
        let current = self.regions.iter().find(|region| region.id == current)?;
        self.regions
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.id != current.id)
            .filter_map(|(order, candidate)| {
                directional_score(current.area, candidate.area, direction, order)
                    .map(|score| (score, candidate.id))
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, id)| id)
    }

    pub fn next(&self, current: Id) -> Option<Id> {
        let current = self.regions.iter().position(|region| region.id == current)?;
        self.regions.get((current + 1) % self.regions.len()).map(|region| region.id)
    }

    pub fn previous(&self, current: Id) -> Option<Id> {
        let current = self.regions.iter().position(|region| region.id == current)?;
        let previous = if current == 0 { self.regions.len() - 1 } else { current - 1 };
        self.regions.get(previous).map(|region| region.id)
    }

    pub fn hit_test(&self, column: u16, row: u16) -> Option<Id> {
        self.regions
            .iter()
            .rev()
            .find(|region| contains(region.area, column, row))
            .map(|region| region.id)
    }

    pub fn normalize(&self, current: Id) -> Option<Id> {
        self.regions
            .iter()
            .find(|region| region.id == current)
            .or_else(|| self.regions.first())
            .map(|region| region.id)
    }
}

type DirectionalScore = (bool, u32, u32, u32, usize);

fn directional_score(
    current: Rect,
    candidate: Rect,
    direction: Direction,
    order: usize,
) -> Option<DirectionalScore> {
    let current_center = center(current);
    let candidate_center = center(candidate);
    let (forward, primary_gap, cross_gap, center_distance) = match direction {
        Direction::Up => (
            candidate_center.1 < current_center.1,
            u32::from(current.y.saturating_sub(candidate.bottom())),
            interval_gap(current.x, current.right(), candidate.x, candidate.right()),
            current_center.0.abs_diff(candidate_center.0),
        ),
        Direction::Down => (
            candidate_center.1 > current_center.1,
            u32::from(candidate.y.saturating_sub(current.bottom())),
            interval_gap(current.x, current.right(), candidate.x, candidate.right()),
            current_center.0.abs_diff(candidate_center.0),
        ),
        Direction::Left => (
            candidate_center.0 < current_center.0,
            u32::from(current.x.saturating_sub(candidate.right())),
            interval_gap(current.y, current.bottom(), candidate.y, candidate.bottom()),
            current_center.1.abs_diff(candidate_center.1),
        ),
        Direction::Right => (
            candidate_center.0 > current_center.0,
            u32::from(candidate.x.saturating_sub(current.right())),
            interval_gap(current.y, current.bottom(), candidate.y, candidate.bottom()),
            current_center.1.abs_diff(candidate_center.1),
        ),
    };
    forward.then_some((cross_gap > 0, primary_gap, cross_gap, center_distance, order))
}

fn center(area: Rect) -> (u32, u32) {
    (u32::from(area.x) + u32::from(area.width) / 2, u32::from(area.y) + u32::from(area.height) / 2)
}

fn interval_gap(start: u16, end: u16, other_start: u16, other_end: u16) -> u32 {
    if end <= other_start {
        u32::from(other_start - end)
    } else if other_end <= start {
        u32::from(start - other_end)
    } else {
        0
    }
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Region {
        Tree,
        Old,
        New,
        Footer,
    }

    fn region(id: Region, x: u16, y: u16, width: u16, height: u16) -> NavigationRegion<Region> {
        NavigationRegion::new(id, Rect::new(x, y, width, height))
    }

    #[test]
    fn arrows_follow_the_rendered_geometry() {
        let map = NavigationMap::new([
            region(Region::Tree, 0, 0, 20, 20),
            region(Region::Old, 20, 0, 30, 20),
            region(Region::New, 50, 0, 30, 20),
            region(Region::Footer, 0, 20, 80, 2),
        ]);

        assert_eq!(map.neighbor(Region::Tree, Direction::Right), Some(Region::Old));
        assert_eq!(map.neighbor(Region::Old, Direction::Right), Some(Region::New));
        assert_eq!(map.neighbor(Region::New, Direction::Left), Some(Region::Old));
        assert_eq!(map.neighbor(Region::Old, Direction::Down), Some(Region::Footer));
        assert_eq!(map.neighbor(Region::Tree, Direction::Up), None);
    }

    #[test]
    fn aligned_regions_beat_closer_diagonal_regions() {
        let map = NavigationMap::new([
            region(Region::Tree, 0, 0, 10, 10),
            region(Region::Old, 30, 0, 10, 10),
            region(Region::New, 11, 11, 10, 10),
        ]);

        assert_eq!(map.neighbor(Region::Tree, Direction::Right), Some(Region::Old));
    }

    #[test]
    fn tab_order_wraps_in_registration_order() {
        let map = NavigationMap::new([
            region(Region::Tree, 0, 0, 10, 10),
            region(Region::Old, 10, 0, 10, 10),
            region(Region::New, 20, 0, 10, 10),
        ]);

        assert_eq!(map.next(Region::Tree), Some(Region::Old));
        assert_eq!(map.next(Region::New), Some(Region::Tree));
        assert_eq!(map.previous(Region::Tree), Some(Region::New));
    }

    #[test]
    fn hidden_regions_are_omitted_and_active_region_is_normalized() {
        let map = NavigationMap::new([
            region(Region::Tree, 0, 0, 10, 10),
            region(Region::Old, 10, 0, 0, 10),
        ]);

        assert_eq!(map.normalize(Region::Old), Some(Region::Tree));
        assert_eq!(map.next(Region::Tree), Some(Region::Tree));
    }

    #[test]
    fn hit_testing_prefers_the_last_rendered_region() {
        let map = NavigationMap::new([
            region(Region::Tree, 0, 0, 10, 10),
            region(Region::Old, 5, 5, 10, 10),
        ]);

        assert_eq!(map.hit_test(2, 2), Some(Region::Tree));
        assert_eq!(map.hit_test(7, 7), Some(Region::Old));
        assert_eq!(map.hit_test(20, 20), None);
    }
}
