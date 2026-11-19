//! Golden test of the `muxr.v1` wire surface.
//!
//! Renders every RPC, message field and enum value of package `muxr.v1` from
//! the build-time descriptor set into a canonical, order- and comment-free
//! listing and compares it with `wire_surface.golden`.

use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet};

const PACKAGE: &str = "muxr.v1";

fn stream(flag: Option<bool>) -> &'static str {
    if flag.unwrap_or(false) { "stream " } else { "" }
}

fn full(type_name: &str) -> &str {
    type_name.strip_prefix('.').unwrap_or(type_name)
}

fn render_enum(e: &EnumDescriptorProto, scope: &str, out: &mut Vec<String>) {
    let name = e.name();
    for v in &e.value {
        out.push(format!("enum {scope}.{name}.{} = {}", v.name(), v.number()));
    }
}

fn render_message(m: &DescriptorProto, scope: &str, out: &mut Vec<String>) {
    let name = format!("{scope}.{}", m.name());
    if m.field.is_empty() {
        out.push(format!("message {name}"));
    }
    for f in &m.field {
        let label = match f.label() {
            Label::Optional => "optional",
            Label::Required => "required",
            Label::Repeated => "repeated",
        };
        let ty = match f.r#type() {
            Type::Double => "double",
            Type::Float => "float",
            Type::Int64 => "int64",
            Type::Uint64 => "uint64",
            Type::Int32 => "int32",
            Type::Fixed64 => "fixed64",
            Type::Fixed32 => "fixed32",
            Type::Bool => "bool",
            Type::String => "string",
            Type::Group => "group",
            Type::Message => "message",
            Type::Bytes => "bytes",
            Type::Uint32 => "uint32",
            Type::Enum => "enum",
            Type::Sfixed32 => "sfixed32",
            Type::Sfixed64 => "sfixed64",
            Type::Sint32 => "sint32",
            Type::Sint64 => "sint64",
        };
        let mut line = format!("field {name}.{} = {} {label} {ty}", f.name(), f.number());
        if let Some(tn) = f.type_name.as_deref() {
            line.push(' ');
            line.push_str(full(tn));
        }
        if let Some(idx) = f.oneof_index {
            line.push_str(&format!(" oneof={}", m.oneof_decl[idx as usize].name()));
        }
        if f.proto3_optional() {
            line.push_str(" proto3_optional");
        }
        out.push(line);
    }
    for nested in &m.nested_type {
        render_message(nested, &name, out);
    }
    for e in &m.enum_type {
        render_enum(e, &name, out);
    }
}

fn render() -> String {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/muxr_descriptor.bin"));
    let set = FileDescriptorSet::decode(&bytes[..]).expect("decode descriptor set");
    let mut lines = Vec::new();
    for file in set.file.iter().filter(|f| f.package() == PACKAGE) {
        for svc in &file.service {
            for m in &svc.method {
                lines.push(format!(
                    "rpc {PACKAGE}.{}/{} ({}{}) returns ({}{})",
                    svc.name(),
                    m.name(),
                    stream(m.client_streaming),
                    full(m.input_type()),
                    stream(m.server_streaming),
                    full(m.output_type()),
                ));
            }
        }
        for m in &file.message_type {
            render_message(m, PACKAGE, &mut lines);
        }
        for e in &file.enum_type {
            render_enum(e, PACKAGE, &mut lines);
        }
    }
    lines.sort();
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[test]
fn wire_surface_matches_golden() {
    let rendered = render();
    if std::env::var_os("UPDATE_WIRE_GOLDEN").is_some() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wire_surface.golden");
        std::fs::write(path, &rendered).expect("write golden");
        return;
    }
    let golden = include_str!("wire_surface.golden");
    assert!(
        rendered == golden,
        "the muxr.v1 wire surface differs from tests/wire_surface.golden.\n\
         A difference here is a wire change; if intended, review it and \
         regenerate the golden by running the test with UPDATE_WIRE_GOLDEN=1.\n\
         --- rendered listing ---\n{rendered}"
    );
}
