//! Cell-aligned split tree. Geometry, hit testing, resize and restore share it.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Axis {
    Columns,
    Rows,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Layout<T> {
    Leaf {
        id: T,
    },
    Split {
        axis: Axis,
        ratio: u16,
        first: Box<Self>,
        second: Box<Self>,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}
impl Rect {
    pub fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Divider {
    pub path: Vec<bool>,
    pub axis: Axis,
    pub rect: Rect,
    pub parent: Rect,
}
impl<T: Copy + Eq> Layout<T> {
    pub fn leaf(id: T) -> Self {
        Self::Leaf { id }
    }
    pub fn ids(&self) -> Vec<T> {
        match self {
            Self::Leaf { id } => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.ids();
                ids.extend(second.ids());
                ids
            }
        }
    }
    pub fn contains(&self, target: T) -> bool {
        self.ids().contains(&target)
    }
    pub fn minimum(&self) -> (usize, usize) {
        match self {
            Self::Leaf { .. } => (8, 3),
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (aw, ah) = first.minimum();
                let (bw, bh) = second.minimum();
                match axis {
                    Axis::Columns => (aw + bw + 1, ah.max(bh)),
                    Axis::Rows => (aw.max(bw), ah + bh + 1),
                }
            }
        }
    }
    pub fn split(&mut self, target: T, id: T, axis: Axis) -> bool {
        match self {
            Self::Leaf { id: old } if *old == target => {
                *self = Self::Split {
                    axis,
                    ratio: 500,
                    first: Box::new(Self::leaf(target)),
                    second: Box::new(Self::leaf(id)),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(target, id, axis) || second.split(target, id, axis)
            }
            _ => false,
        }
    }
    pub fn filter_map<U: Copy + Eq>(
        &self,
        map: &mut impl FnMut(T) -> Option<U>,
    ) -> Option<Layout<U>> {
        match self {
            Self::Leaf { id } => map(*id).map(Layout::leaf),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.filter_map(map), second.filter_map(map)) {
                (Some(a), Some(b)) => Some(Layout::Split {
                    axis: *axis,
                    ratio: (*ratio).clamp(50, 950),
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
        }
    }
    pub fn remove(&self, id: T) -> Option<Self> {
        self.filter_map(&mut |old| (old != id).then_some(old))
    }
    pub fn geometry(&self, rect: Rect) -> (Vec<(T, Rect)>, Vec<Divider>) {
        let mut leaves = Vec::new();
        let mut dividers = Vec::new();
        self.visit(rect, &mut Vec::new(), &mut leaves, &mut dividers);
        (leaves, dividers)
    }
    fn visit(
        &self,
        rect: Rect,
        path: &mut Vec<bool>,
        leaves: &mut Vec<(T, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Leaf { id } => leaves.push((*id, rect)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let extent = match axis {
                    Axis::Columns => rect.w,
                    Axis::Rows => rect.h,
                };
                if extent < 3 {
                    return;
                }
                let available = extent - 1;
                let (a, b) = match axis {
                    Axis::Columns => (first.minimum().0, second.minimum().0),
                    Axis::Rows => (first.minimum().1, second.minimum().1),
                };
                let low = if available >= a + b { a } else { 1 };
                let high = if available >= a + b {
                    available - b
                } else {
                    available - 1
                };
                let cut = (available * (*ratio as usize) / 1000).clamp(low, high);
                let (r1, line, r2) = match axis {
                    Axis::Columns => (
                        Rect { w: cut, ..rect },
                        Rect {
                            x: rect.x + cut,
                            w: 1,
                            ..rect
                        },
                        Rect {
                            x: rect.x + cut + 1,
                            w: available - cut,
                            ..rect
                        },
                    ),
                    Axis::Rows => (
                        Rect { h: cut, ..rect },
                        Rect {
                            y: rect.y + cut,
                            h: 1,
                            ..rect
                        },
                        Rect {
                            y: rect.y + cut + 1,
                            h: available - cut,
                            ..rect
                        },
                    ),
                };
                dividers.push(Divider {
                    path: path.clone(),
                    axis: *axis,
                    rect: line,
                    parent: rect,
                });
                path.push(false);
                first.visit(r1, path, leaves, dividers);
                path.pop();
                path.push(true);
                second.visit(r2, path, leaves, dividers);
                path.pop();
            }
        }
    }
    pub fn set_ratio(&mut self, path: &[bool], ratio: u16) {
        if let Self::Split {
            ratio: current,
            first,
            second,
            ..
        } = self
        {
            if let Some((&side, tail)) = path.split_first() {
                if side {
                    second.set_ratio(tail, ratio)
                } else {
                    first.set_ratio(tail, ratio)
                }
            } else {
                *current = ratio.clamp(50, 950);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_split_hit_testing_resize_and_collapse() {
        let mut root = Layout::leaf(1);
        assert!(root.split(1, 2, Axis::Columns));
        assert!(root.split(2, 3, Axis::Rows));
        let bounds = Rect {
            x: 0,
            y: 0,
            w: 81,
            h: 25,
        };
        let (leaves, lines) = root.geometry(bounds);
        assert_eq!(leaves.len(), 3);
        assert_eq!(lines.len(), 2);
        for y in 0..25 {
            for x in 0..81 {
                assert_eq!(
                    leaves.iter().filter(|(_, r)| r.contains(x, y)).count()
                        + lines.iter().filter(|d| d.rect.contains(x, y)).count(),
                    1
                );
            }
        }
        root.set_ratio(&[], 750);
        assert_eq!(root.geometry(bounds).0[0].1.w, 60);
        let root = root.remove(2).unwrap();
        assert_eq!(root.ids(), vec![1, 3]);
        let root = root.remove(1).unwrap();
        assert_eq!(root, Layout::leaf(3));
    }
    #[test]
    fn restored_failures_collapse_without_duplicating_ids() {
        let mut root = Layout::leaf(0);
        root.split(0, 1, Axis::Rows);
        root.split(1, 2, Axis::Columns);
        assert_eq!(
            root.filter_map(&mut |id| (id == 2).then_some(9)),
            Some(Layout::leaf(9))
        );
        let encoded = toml::to_string(&root).unwrap();
        assert_eq!(toml::from_str::<Layout<usize>>(&encoded).unwrap(), root);
    }
}
