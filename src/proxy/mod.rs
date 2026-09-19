//! 反向代理模块。设计见 docs/PLAN-REVERSE-PROXY.md。
pub mod forward;
pub mod router;

// 编译期断言:hyper/hyper-util 在本 feature 下作为依赖可见。
// (计划原文的 `Client::builder` 函数指针写法在本版本编译不过——泛型关联函数
//  无法 coerce 成非泛型 fn 指针,且 Exec 未公开 re-export;断言改为公开类型,
//  Client 构造的真实可用性由 forward.rs 实现与 e2e 验证。)
const _: fn() = || {
    fn _assert_hyper() -> hyper::Request<hyper::body::Incoming> {
        unreachable!()
    }
    fn _assert_hyper_util() -> hyper_util::client::legacy::connect::HttpConnector {
        unreachable!()
    }
};
