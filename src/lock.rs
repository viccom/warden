//! 锁毒化恢复:持锁 panic 后取回内部数据继续服务,而非连锁 panic。
//!
//! warden 是常驻监护 daemon,锁内数据(监护状态/日志缓冲/配置快照)均为
//! 可自愈数据;任一处持锁 panic 毒化锁后,若沿用 `unwrap()` 会让后续所有
//! 经过该锁的请求连锁 panic——对监护进程是过度反应(parking_lot 即无毒化
//! 语义,此处以 `into_inner` 达到同等效果)。恢复拿到的是 panic 现场的
//! 半程状态,不保证不变量完好,故恢复时记 warn 供现场定位根因;
//! 这是纵深防御,代码仍不应在持锁时 panic。

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Mutex 加锁;毒化时取回内部数据并 warn(状态可能不一致,但优于连锁 panic)。
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("[lock] Mutex 曾因持锁 panic 毒化,已取回数据继续服务");
            e.into_inner()
        }
    }
}

/// RwLock 读锁;毒化语义同 [`lock`]。
pub fn read<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    match m.read() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("[lock] RwLock 曾因持锁 panic 毒化,已取回数据继续服务");
            e.into_inner()
        }
    }
}

/// RwLock 写锁;毒化语义同 [`lock`]。
pub fn write<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    match m.write() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("[lock] RwLock 曾因持锁 panic 毒化,已取回数据继续服务");
            e.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 意图:持锁 panic 毒化后,lock() 仍取回内部数据而非连锁 panic。
    #[test]
    fn lock_recovers_from_poisoned_mutex() {
        let m = Mutex::new(vec![1]);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
            panic!("持锁 panic 毒化");
        }));
        assert!(m.is_poisoned());
        let mut g = lock(&m);
        g.push(2);
        assert_eq!(&*g, &[1, 2], "毒化后应取回原数据继续可变访问");
    }

    /// 意图:RwLock 读写两路同样从毒化中恢复。
    #[test]
    fn rwlock_recovers_from_poison() {
        let rw = RwLock::new(1u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = rw.write().unwrap();
            panic!("持写锁 panic 毒化");
        }));
        assert!(rw.is_poisoned());
        *write(&rw) = 3;
        assert_eq!(*read(&rw), 3, "毒化后读写应恢复而非 panic");
    }
}
