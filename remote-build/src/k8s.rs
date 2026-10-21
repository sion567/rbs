//! k8s 支持（设计文档 §8）。
//!
//! 核心结论（§8.1）：k8s 不需要特殊对待——kubectl / helm 都是普通工具命令，
//! TargetType::Kubernetes 只需注入 KUBECONFIG（引擎 engine.rs 已处理，§8.4）。
//!
//! P0：k8s-apply 步骤类型退化为普通 exec 执行 kubectl 命令。
//! P1.5：K8sApply 步骤的 k8s_watch 轮询（§8.3）在此模块实现。
