use crate::spec::{
    base, LinkerFlavor, Lld, SanitizerSet, StackProbeType, Target, TargetOptions, Cc, LinkSelfContainedDefault, TlsModel
};

pub fn target() -> Target {
    let mut base = base::linux_musl::opts();
    base.max_atomic_width = Some(128);
    base.supports_xray = true;
    base.features = "+v8a".into();
    base.stack_probes = StackProbeType::Inline;
    base.supported_sanitizers = SanitizerSet::ADDRESS
        | SanitizerSet::CFI
        | SanitizerSet::LEAK
        | SanitizerSet::MEMORY
        | SanitizerSet::THREAD;
    base.requires_lto = true;
    base.obj_is_bitcode = true;
    base.linker = Some("aarch64-lfi-linux-musl-gcc".into());
    base.post_link_args = TargetOptions::link_args(LinkerFlavor::Gnu(Cc::Yes, Lld::No), &["-lgcc_eh"]);
    base.static_position_independent_executables = true;
    base.link_self_contained = LinkSelfContainedDefault::False;
    base.tls_model = TlsModel::InitialExec;

    Target {
        llvm_target: "aarch64-lfi-linux-musl".into(),
        metadata: crate::spec::TargetMetadata {
            description: Some("ARM64 LFI with musl 1.2.3".into()),
            tier: Some(3),
            host_tools: Some(true),
            std: Some(true),
        },
        pointer_width: 64,
        data_layout: "e-m:e-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32".into(),
        arch: "aarch64".into(),
        options: TargetOptions { mcount: "\u{1}_mcount".into(), ..base },
    }
}
