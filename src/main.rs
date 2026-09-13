// protoc-gen-easyrpc-rust: read CodeGeneratorRequest and emit a `*_easyrpc.rs`
// with a METHOD_SPECS table. Message types come from prost.
//
// REST paths: we use prost-reflect to resolve the `google.api.http` extension
// option (field number 72295728) on each method. prost-types drops unknown /
// extension option bytes on decode, so we deliberately build the
// `FileDescriptorSet` straight from the RAW CodeGeneratorRequest bytes (each
// proto_file payload is copied verbatim and rewrapped as field 1). That way the
// extension payload is preserved, and `DescriptorPool::decode` can expose it as
// a typed `google.api.http` HttpRule. We then read the verb (get/put/post/
// delete/patch) and its path by field number/name via dynamic reflection.
use std::io::{Read, Write};

use prost::Message;
use prost_reflect::DescriptorPool;
use prost_types::compiler::{CodeGeneratorRequest, CodeGeneratorResponse, code_generator_response};

const GOOGLE_API_HTTP: &str = "google.api.http";

fn main() {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).expect("read stdin");
    let req = CodeGeneratorRequest::decode(&input[..]).expect("decode CodeGeneratorRequest");

    // Build a FileDescriptorSet from the raw proto_file payloads (field 15 of
    // CodeGeneratorRequest), copying bytes verbatim so extension options are
    // not stripped by any prost_types round-trip.
    let fds_bytes = raw_file_descriptor_set(&input);

    // DescriptorPool::decode (not from_file_descriptor_set) preserves extension
    // options, which lets us resolve google.api.http on method options.
    let pool = DescriptorPool::decode(fds_bytes.as_slice()).expect("build descriptor pool");

    let mut files = Vec::new();
    for name in &req.file_to_generate {
        let Some(file) = pool.get_file_by_name(name) else { continue };
        let methods = collect_methods(&pool, &file);
        if methods.is_empty() { continue }
        files.push(code_generator_response::File {
            name: Some(format!("{}_easyrpc.rs", name.trim_end_matches(".proto"))),
            content: Some(render(&methods)),
            ..Default::default()
        });
    }

    let resp = CodeGeneratorResponse { file: files, supported_features: Some(code_generator_response::Feature::Proto3Optional as u64), ..Default::default() };
    std::io::stdout().write_all(&resp.encode_to_vec()).expect("write stdout");
}

struct M {
    service: String,
    name: String,
    path: String,
    http_method: String,
    client_stream: bool,
    server_stream: bool,
}

/// Rewrap each CodeGeneratorRequest.proto_file (field 15) payload verbatim as a
/// FileDescriptorSet.file (field 1) so extension option bytes survive.
fn raw_file_descriptor_set(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let (tag, ni) = read_var(input, i).unwrap_or((0, input.len()));
        i = ni;
        let field = tag >> 3;
        let wt = tag & 7;
        if wt == 2 {
            let (len, ni2) = read_var(input, i).unwrap_or((0, input.len()));
            i = ni2;
            if field == 15 {
                let payload = &input[i..i + len as usize];
                out.push(0x0a); // FileDescriptorSet.file = field 1, length-delimited
                out.extend_from_slice(&varint_len(len));
                out.extend_from_slice(payload);
            }
            i += len as usize;
        } else {
            i = skip_wire(input, i, wt);
        }
    }
    out
}

fn collect_methods(pool: &DescriptorPool, file: &prost_reflect::FileDescriptor) -> Vec<M> {
    let ext = pool.get_extension_by_name(GOOGLE_API_HTTP);
    let mut out = Vec::new();
    for service in file.services() {
        let service_name = service.full_name().to_string();
        for method in service.methods() {
            let name = method.name().to_string();
            let (path, verb) = method
                .options()
                .rest_path(ext.as_ref())
                .unwrap_or_else(|| (format!("/{}/{}", service_name, name), "POST".to_string()));
            out.push(M {
                service: service_name.clone(),
                name,
                path,
                http_method: verb,
                client_stream: method.method_descriptor_proto().client_streaming.unwrap_or(false),
                server_stream: method.method_descriptor_proto().server_streaming.unwrap_or(false),
            });
        }
    }
    out
}

/// Read (path, verb) from a method's google.api.http extension option.
///
/// HttpRule verb fields (in order): get=2 put=3 post=4 delete=5 patch=6. We
/// resolve by polling those field numbers on the dynamically-decoded HttpRule
/// message, mirroring the oneof `pattern` in google.api.HttpRule.
trait RestPath {
    fn rest_path(&self, ext: Option<&prost_reflect::ExtensionDescriptor>) -> Option<(String, String)>;
}

impl RestPath for prost_reflect::DynamicMessage {
    fn rest_path(&self, ext: Option<&prost_reflect::ExtensionDescriptor>) -> Option<(String, String)> {
        let ext = ext?;
        if !self.has_extension(ext) {
            return None;
        }
        let rule = self.get_extension(ext);
        let rule_msg = rule.as_message()?;
        for (number, verb) in [(2u32, "GET"), (3, "PUT"), (4, "POST"), (5, "DELETE"), (6, "PATCH")] {
            if let Some(value) = rule_msg.get_field_by_number(number) {
                if let Some(path) = value.as_str() {
                    if !path.is_empty() {
                        return Some((path.to_string(), verb.to_string()));
                    }
                }
            }
        }
        None
    }
}

fn render(methods: &[M]) -> String {
    let mut s = String::new();
    s.push_str("// Code generated by protoc-gen-easyrpc-rust. DO NOT EDIT.\n");
    s.push_str("pub fn method_specs() -> Vec<crate::protocol::MethodSpec> {\n  vec![\n");
    for m in methods {
        s.push_str(&format!(
            "  crate::protocol::MethodSpec {{ service: {:?}.to_string(), name: {:?}.to_string(), path: {:?}.to_string(), http_method: {:?}.to_string(), client_stream: {}, server_stream: {}, body: String::new() }},\n",
            m.service, m.name, m.path, m.http_method, m.client_stream, m.server_stream
        ));
    }
    s.push_str("  ]\n}\n");
    s
}

fn read_var(d: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut v = 0u64; let mut s = 0u32;
    loop {
        let b = *d.get(i)?; i += 1;
        v |= ((b & 0x7f) as u64) << s; s += 7;
        if b & 0x80 == 0 { return Some((v, i)) }
        if s > 63 { return None }
    }
}

fn skip_wire(data: &[u8], mut i: usize, wt: u64) -> usize {
    match wt {
        0 => { if let Some((_, ni)) = read_var(data, i) { i = ni } }
        2 => { if let Some((len, ni)) = read_var(data, i) { i = ni + len as usize } }
        5 => i += 4,
        1 => i += 8,
        _ => {}
    }
    i
}

fn varint_len(mut v: u64) -> Vec<u8> {
    let mut o = Vec::new();
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 { o.push(b | 0x80) } else { o.push(b); break }
    }
    o
}
