//! RetiredSlot:旧渲染器的延后释放槽 —— 移出呼出关键路径。

/// 单槽延后释放:replace 立即 drop 旧值;take_and_drop 在事件循环
/// 空闲点(AboutToWait)显式释放,把同步 GPU 销毁移出呼出热路径。
pub struct RetiredSlot<T> {
    slot: Option<T>,
}

impl<T> Default for RetiredSlot<T> {
    fn default() -> Self {
        Self { slot: None }
    }
}

impl<T> RetiredSlot<T> {
    /// 放入新值(旧值立即 drop)。有界:同一时刻最多保留一个。
    pub fn replace(&mut self, v: T) {
        self.slot = Some(v);
    }

    /// 显式释放当前值。
    pub fn take_and_drop(&mut self) {
        self.slot = None;
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropT(Arc<AtomicUsize>);

    impl Drop for DropT {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn replace_drops_previous_immediately() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = RetiredSlot::default();
        slot.replace(DropT(drops.clone()));
        slot.replace(DropT(drops.clone()));
        assert_eq!(drops.load(Ordering::SeqCst), 1, "被替换的旧值必须立即 drop");
    }

    #[test]
    fn take_and_drop_empties_slot() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = RetiredSlot::default();
        slot.replace(DropT(drops.clone()));
        slot.take_and_drop();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn take_and_drop_on_empty_is_noop() {
        let mut slot = RetiredSlot::<DropT>::default();
        slot.take_and_drop();
    }
}
