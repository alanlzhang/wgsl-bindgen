//! # wgsl_bindgen
//! wgsl_bindgen is an experimental library for generating typesafe Rust bindings from WGSL shaders to [wgpu](https://github.com/gfx-rs/wgpu).
//!
//! ## Features
//! The `WgslBindgenOptionBuilder` is used to configure the generation of Rust bindings from WGSL shaders. This facilitates a shader focused workflow where edits to WGSL code are automatically reflected in the corresponding Rust file. For example, changing the type of a uniform in WGSL will raise a compile error in Rust code using the generated struct to initialize the buffer.
//!
//! Writing Rust code to interact with WGSL shaders can be tedious and error prone, especially when the types and functions in the shader code change during development. wgsl_bindgen is not a rendering library and does not offer high level abstractions like a scene graph or material system. However, using generated code still has a number of advantages compared to writing the code by hand.
//!
//! The code generated by wgsl_bindgen can help with valid API usage like:
//! - setting all bind groups and bind group bindings
//! - setting correct struct fields and field types for vertex input buffers
//! - setting correct struct struct fields and field types for storage and uniform buffers
//! - configuring shader initialization
//! - getting vertex attribute offsets for vertex buffers
//! - const validation of struct memory layouts when using bytemuck
//!
//! Here's an example of how to use `WgslBindgenOptionBuilder` to generate Rust bindings from WGSL shaders:
//!
//! ```no_run
//! use miette::{IntoDiagnostic, Result};
//! use wgsl_bindgen::{WgslTypeSerializeStrategy, WgslBindgenOptionBuilder, GlamWgslTypeMap};
//!
//! fn main() -> Result<()> {
//!     WgslBindgenOptionBuilder::default()
//!         .add_entry_point("src/shader/testbed.wgsl")
//!         .add_entry_point("src/shader/triangle.wgsl")
//!         .skip_hash_check(true)
//!         .serialization_strategy(WgslTypeSerializeStrategy::Bytemuck)
//!         .wgsl_type_map(GlamWgslTypeMap)
//!         .derive_serde(false)
//!         .output_file("src/shader.rs")
//!         .build()?
//!         .generate()
//!         .into_diagnostic()
//! }
//! ```

#[allow(dead_code, unused)]
extern crate wgpu_types as wgpu;

use bevy_util::SourceWithFullDependenciesResult;
use bindgroup::{bind_groups_module, get_bind_group_data};
use case::CaseExt;
use derive_more::IsVariant;
use naga::ShaderStage;
use naga_util::module_to_source;
use proc_macro2::{Literal, Span, TokenStream};
use quote::quote;
use quote_gen::{
  add_prelude_types_assertions, create_shader_raw_string_literal, RustModBuilder,
  MOD_REFERENCE_ROOT,
};
use syn::{Ident, Index};
use thiserror::Error;

pub mod bevy_util;
mod bindgroup;
mod consts;
mod naga_util;
mod quote_gen;
mod structs;
mod types;
mod wgsl;
mod wgsl_bindgen;
mod wgsl_type;

pub use types::*;
pub use wgsl_bindgen::*;
pub use wgsl_type::*;

/// Enum representing the possible serialization strategies for WGSL types.
///
/// This enum is used to specify how WGSL types should be serialized when converted
/// to Rust types.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, IsVariant)]
pub enum WgslTypeSerializeStrategy {
  #[default]
  Encase,
  Bytemuck,
}

/// Errors while generating Rust source for a WGSl shader module.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum CreateModuleError {
  /// Bind group sets must be consecutive and start from 0.
  /// See `bind_group_layouts` for
  /// [PipelineLayoutDescriptor](https://docs.rs/wgpu/latest/wgpu/struct.PipelineLayoutDescriptor.html#).
  #[error("bind groups are non-consecutive or do not start from 0")]
  NonConsecutiveBindGroups,

  /// Each binding resource must be associated with exactly one binding index.
  #[error("duplicate binding found with index `{binding}`")]
  DuplicateBinding { binding: u32 },
}

pub(crate) struct WgslEntryResult<'a> {
  mod_name: String,
  naga_module: naga::Module,
  source_including_deps: SourceWithFullDependenciesResult<'a>,
}

fn create_rust_bindings(
  entries: Vec<WgslEntryResult<'_>>,
  options: &WgslBindgenOption,
) -> Result<String, CreateModuleError> {
  let mut mod_builder = RustModBuilder::new(true);
  mod_builder.add(MOD_REFERENCE_ROOT, add_prelude_types_assertions(options));

  for entry in entries.iter() {
    let WgslEntryResult {
      mod_name,
      naga_module,
      ..
    } = entry;
    let bind_group_data = get_bind_group_data(naga_module)?;
    let shader_stages = wgsl::shader_stages(naga_module);

    // Write all the structs, including uniforms and entry function inputs.
    mod_builder
      .add_items(mod_name, structs::structs_items(naga_module, options))
      .unwrap();

    mod_builder
      .add_items(mod_name, consts::consts_items(naga_module))
      .unwrap();

    mod_builder.add(mod_name, bind_groups_module(&bind_group_data, shader_stages));
    mod_builder.add(mod_name, vertex_struct_methods(naga_module));

    mod_builder.add(mod_name, compute_module(naga_module));
    mod_builder.add(mod_name, entry_point_constants(naga_module));
    mod_builder.add(mod_name, vertex_states(naga_module));

    let bind_group_layouts: Vec<_> = bind_group_data
      .keys()
      .map(|group_no| {
        let group = indexed_name_to_ident("BindGroup", *group_no);
        quote!(bind_groups::#group::get_bind_group_layout(device))
      })
      .collect();

    let create_pipeline_layout = quote! {
        pub fn create_pipeline_layout(device: &wgpu::Device) -> wgpu::PipelineLayout {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[
                    #(&#bind_group_layouts),*
                ],
                push_constant_ranges: &[],
            })
        }
    };

    mod_builder.add(mod_name, create_pipeline_layout);
    mod_builder.add(mod_name, shader_module(entry, options));
  }

  let output = mod_builder.generate();
  Ok(pretty_print(&output))
}

fn pretty_print(tokens: &TokenStream) -> String {
  let file = syn::parse_file(&tokens.to_string()).unwrap();
  prettyplease::unparse(&file)
  // tokens.to_string()
}

fn indexed_name_to_ident(name: &str, index: u32) -> Ident {
  Ident::new(&format!("{name}{index}"), Span::call_site())
}

fn shader_module_using_final_shader_string(entry: &WgslEntryResult) -> TokenStream {
  let shader_content = module_to_source(&entry.naga_module).unwrap();
  let shader_literal = create_shader_raw_string_literal(&shader_content);
  let create_shader_module = quote! {
      pub fn create_shader_module(device: &wgpu::Device) -> wgpu::ShaderModule {
          let source = std::borrow::Cow::Borrowed(SHADER_STRING);
          device.create_shader_module(wgpu::ShaderModuleDescriptor {
              label: None,
              source: wgpu::ShaderSource::Wgsl(source)
          })
      }
  };
  let shader_str_def = quote!(const SHADER_STRING: &'static str = #shader_literal;);

  quote! {
    #create_shader_module
    #shader_str_def
  }
}

fn shader_module_using_composer(
  entry: &WgslEntryResult,
  options: &WgslBindgenOption,
) -> TokenStream {
  let output_dir = options
    .output_file
    .as_ref()
    .and_then(|output_file| output_file.parent().map(|p| p.to_path_buf()))
    .unwrap_or_else(|| {
      std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".into())
        .into()
    });

  let get_relative_path = |file: &SourceFilePath| -> String {
    let relative_path = pathdiff::diff_paths(file.as_path(), &output_dir)
      .expect("failed to get relative path");
    relative_path.to_str().unwrap().to_string()
  };

  let add_shader_modules_token_stream = entry
    .source_including_deps
    .full_dependencies
    .iter()
    .map(|dep| {
      let relative_file_path = get_relative_path(&dep.file_path);
      let as_name = dep.module_name.as_ref().map(|name| name.to_string());

      let as_name_assignment = match as_name {
        Some(as_name) => quote! { as_name: Some(#as_name.into()) },
        None => quote!(),
      };

      quote! {
        composer.add_composable_module(
          naga_oil::compose::ComposableModuleDescriptor {
            source: include_str!(#relative_file_path),
            file_path: #relative_file_path,
            language: naga_oil::compose::ShaderLanguage::Wgsl,
            #as_name_assignment,
            ..Default::default()
          }
        ).expect("failed to add composer module");
      }
    })
    .collect::<Vec<_>>();

  let entry_relative_path =
    get_relative_path(&entry.source_including_deps.source_file.file_path);

  quote! {
    pub fn init_composer() -> naga_oil::compose::Composer {
      #[allow(unused_mut)]
      let mut composer = naga_oil::compose::Composer::default();
      #(#add_shader_modules_token_stream)*
      composer
    }

    pub fn make_naga_module(composer: &mut naga_oil::compose::Composer) -> wgpu::naga::Module {
      composer.make_naga_module(naga_oil::compose::NagaModuleDescriptor {
        source: include_str!(#entry_relative_path),
        file_path: #entry_relative_path,
        ..Default::default()
      }).expect("failed to build naga module")
    }

    pub fn naga_module_to_string(module: &wgpu::naga::Module) -> String {
        // Mini validation to get module info
      let info = wgpu::naga::valid::Validator::new(
        wgpu::naga::valid::ValidationFlags::empty(),
        wgpu::naga::valid::Capabilities::all(),
      )
      .validate(&module);

      // Write to wgsl
      let info = info.unwrap();

      wgpu::naga::back::wgsl::write_string(
        &module,
        &info,
        wgpu::naga::back::wgsl::WriterFlags::empty(),
      ).expect("failed to convert naga module to source")
    }

    pub fn create_shader_module(device: &wgpu::Device) -> wgpu::ShaderModule {
      let mut composer = init_composer();
      let module = make_naga_module(&mut composer);

      let source = naga_module_to_string(&module);
      let source = std::borrow::Cow::Owned(source);
      device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(source)
      })
    }
  }
}

fn shader_module(entry: &WgslEntryResult, options: &WgslBindgenOption) -> TokenStream {
  match options.shader_source_output_type {
    WgslShaderSourceOutputType::FinalShaderString => {
      shader_module_using_final_shader_string(entry)
    }
    WgslShaderSourceOutputType::Composer => shader_module_using_composer(entry, options),
  }
}

fn compute_module(module: &naga::Module) -> TokenStream {
  let entry_points: Vec<_> = module
    .entry_points
    .iter()
    .filter_map(|e| {
      if e.stage == naga::ShaderStage::Compute {
        let workgroup_size_constant = workgroup_size(e);
        let create_pipeline = create_compute_pipeline(e);

        Some(quote! {
            #workgroup_size_constant
            #create_pipeline
        })
      } else {
        None
      }
    })
    .collect();

  if entry_points.is_empty() {
    // Don't include empty modules.
    quote!()
  } else {
    quote! {
        pub mod compute {
            #(#entry_points)*
        }
    }
  }
}

fn create_compute_pipeline(e: &naga::EntryPoint) -> TokenStream {
  // Compute pipeline creation has few parameters and can be generated.
  let pipeline_name =
    Ident::new(&format!("create_{}_pipeline", e.name), Span::call_site());
  let entry_point = &e.name;
  // TODO: Include a user supplied module name in the label?
  let label = format!("Compute Pipeline {}", e.name);
  quote! {
      pub fn #pipeline_name(device: &wgpu::Device) -> wgpu::ComputePipeline {
          let module = super::create_shader_module(device);
          let layout = super::create_pipeline_layout(device);
          device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
              label: Some(#label),
              layout: Some(&layout),
              module: &module,
              entry_point: #entry_point,
          })
      }
  }
}

fn workgroup_size(e: &naga::EntryPoint) -> TokenStream {
  // Use Index to avoid specifying the type on literals.
  let name =
    Ident::new(&format!("{}_WORKGROUP_SIZE", e.name.to_uppercase()), Span::call_site());
  let [x, y, z] = e.workgroup_size.map(|s| Index::from(s as usize));
  quote!(pub const #name: [u32; 3] = [#x, #y, #z];)
}

fn vertex_struct_methods(module: &naga::Module) -> TokenStream {
  let structs = vertex_input_structs(module);
  quote!(#(#structs)*)
}

fn entry_point_constants(module: &naga::Module) -> TokenStream {
  let entry_points: Vec<TokenStream> = module
    .entry_points
    .iter()
    .map(|entry_point| {
      let entry_name = Literal::string(&entry_point.name);
      let const_name = Ident::new(
        &format!("ENTRY_{}", &entry_point.name.to_uppercase()),
        Span::call_site(),
      );
      quote! {
          pub const #const_name: &str = #entry_name;
      }
    })
    .collect();

  quote! {
      #(#entry_points)*
  }
}

fn vertex_states(module: &naga::Module) -> TokenStream {
  let vertex_inputs = wgsl::get_vertex_input_structs(module);
  let mut step_mode_params = vec![];
  let layout_expressions: Vec<TokenStream> = vertex_inputs
    .iter()
    .map(|input| {
      let name = Ident::new(&input.name, Span::call_site());
      let step_mode = Ident::new(&input.name.to_snake(), Span::call_site());
      step_mode_params.push(quote!(#step_mode: wgpu::VertexStepMode));
      quote!(#name::vertex_buffer_layout(#step_mode))
    })
    .collect();

  let vertex_entries: Vec<TokenStream> = module
    .entry_points
    .iter()
    .filter_map(|entry_point| match &entry_point.stage {
      ShaderStage::Vertex => {
        let fn_name =
          Ident::new(&format!("{}_entry", &entry_point.name), Span::call_site());
        let const_name = Ident::new(
          &format!("ENTRY_{}", &entry_point.name.to_uppercase()),
          Span::call_site(),
        );
        let n = vertex_inputs.len();
        let n = Literal::usize_unsuffixed(n);
        Some(quote! {
            pub fn #fn_name(#(#step_mode_params),*) -> VertexEntry<#n> {
                VertexEntry {
                    entry_point: #const_name,
                    buffers: [
                        #(#layout_expressions),*
                    ]
                }
            }
        })
      }
      _ => None,
    })
    .collect();

  // Don't generate unused code.
  if vertex_entries.is_empty() {
    quote!()
  } else {
    quote! {
        #[derive(Debug)]
        pub struct VertexEntry<const N: usize> {
            entry_point: &'static str,
            buffers: [wgpu::VertexBufferLayout<'static>; N]
        }

        pub fn vertex_state<'a, const N: usize>(
            module: &'a wgpu::ShaderModule,
            entry: &'a VertexEntry<N>,
        ) -> wgpu::VertexState<'a> {
            wgpu::VertexState {
                module,
                entry_point: entry.entry_point,
                buffers: &entry.buffers,
            }
        }

        #(#vertex_entries)*
    }
  }
}

fn vertex_input_structs(module: &naga::Module) -> Vec<TokenStream> {
  let vertex_inputs = wgsl::get_vertex_input_structs(module);
  vertex_inputs.iter().map(|input|  {
        let name = Ident::new(&input.name, Span::call_site());

        // Use index to avoid adding prefix to literals.
        let count = Index::from(input.fields.len());
        let attributes: Vec<_> = input
            .fields
            .iter()
            .map(|(location, m)| {
                let field_name: TokenStream = m.name.as_ref().unwrap().parse().unwrap();
                let location = Index::from(*location as usize);
                let format = wgsl::vertex_format(&module.types[m.ty]);
                // TODO: Will the debug implementation always work with the macro?
                let format = Ident::new(&format!("{format:?}"), Span::call_site());

                quote! {
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::#format,
                        offset: std::mem::offset_of!(#name, #field_name) as u64,
                        shader_location: #location,
                    }
                }
            })
            .collect();


        // The vertex_attr_array! macro doesn't account for field alignment.
        // Structs with glam::Vec4 and glam::Vec3 fields will not be tightly packed.
        // Manually calculate the Rust field offsets to support using bytemuck for vertices.
        // This works since we explicitly mark all generated structs as repr(C).
        // Assume elements are in Rust arrays or slices, so use size_of for stride.
        // TODO: Should this enforce WebGPU alignment requirements for compatibility?
        // https://gpuweb.github.io/gpuweb/#abstract-opdef-validating-gpuvertexbufferlayout

        // TODO: Support vertex inputs that aren't in a struct.
        quote! {
            impl #name {
                pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; #count] = [#(#attributes),*];

                pub const fn vertex_buffer_layout(step_mode: wgpu::VertexStepMode) -> wgpu::VertexBufferLayout<'static> {
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<#name>() as u64,
                        step_mode,
                        attributes: &#name::VERTEX_ATTRIBUTES
                    }
                }
            }
        }
    }).collect()
}

// Tokenstreams can't be compared directly using PartialEq.
// Use pretty_print to normalize the formatting and compare strings.
// Use a colored diff output to make differences easier to see.
#[cfg(test)]
#[macro_export]
macro_rules! assert_tokens_eq {
  ($a:expr, $b:expr) => {
    pretty_assertions::assert_eq!(crate::pretty_print(&$a), crate::pretty_print(&$b))
  };
}

#[cfg(test)]
mod test {
  use indoc::indoc;

  use self::bevy_util::source_file::SourceFile;
  use super::*;

  fn create_shader_module(
    source: &str,
    options: WgslBindgenOption,
  ) -> Result<String, CreateModuleError> {
    let naga_module = naga::front::wgsl::parse_str(source).unwrap();
    let dummy_source = SourceFile::create(SourceFilePath::new(""), None, "".into());
    let entry = WgslEntryResult {
      mod_name: "test".into(),
      naga_module,
      source_including_deps: SourceWithFullDependenciesResult {
        full_dependencies: Default::default(),
        source_file: &dummy_source,
      },
    };

    create_rust_bindings(vec![entry], &options)
  }

  #[test]
  fn create_shader_module_embed_source() {
    let source = indoc! {r#"
            @fragment
            fn fs_main() {}
        "#};

    let actual = create_shader_module(source, WgslBindgenOption::default()).unwrap();

    pretty_assertions::assert_eq!(
      indoc! {r##"
                #[allow(unused)]
                mod _root {
                    pub use super::*;
                }
                pub mod test {
                    #[allow(unused_imports)]
                    use super::{_root, _root::*};
                    pub const ENTRY_FS_MAIN: &str = "fs_main";
                    pub fn create_pipeline_layout(device: &wgpu::Device) -> wgpu::PipelineLayout {
                        device
                            .create_pipeline_layout(
                                &wgpu::PipelineLayoutDescriptor {
                                    label: None,
                                    bind_group_layouts: &[],
                                    push_constant_ranges: &[],
                                },
                            )
                    }
                    pub fn create_shader_module(device: &wgpu::Device) -> wgpu::ShaderModule {
                        let source = std::borrow::Cow::Borrowed(SHADER_STRING);
                        device
                            .create_shader_module(wgpu::ShaderModuleDescriptor {
                                label: None,
                                source: wgpu::ShaderSource::Wgsl(source),
                            })
                    }
                    const SHADER_STRING: &'static str = r#"
                @fragment
                fn fs_main() {
                    return;
                }
                "#;
                }
            "##},
      actual
    );
  }

  #[test]
  fn create_shader_module_consecutive_bind_groups() {
    let source = indoc! {r#"
            struct A {
                f: vec4<f32>
            };
            @group(0) @binding(0) var<uniform> a: A;
            @group(1) @binding(0) var<uniform> b: A;

            @vertex
            fn vs_main() -> @builtin(position) vec4<f32> {
              return vec4<f32>(0.0, 0.0, 0.0, 1.0);
            }

            @fragment
            fn fs_main() {}
        "#};

    create_shader_module(source, WgslBindgenOption::default()).unwrap();
  }

  #[test]
  fn create_shader_module_non_consecutive_bind_groups() {
    let source = indoc! {r#"
            @group(0) @binding(0) var<uniform> a: vec4<f32>;
            @group(1) @binding(0) var<uniform> b: vec4<f32>;
            @group(3) @binding(0) var<uniform> c: vec4<f32>;

            @fragment
            fn main() {}
        "#};

    let result = create_shader_module(source, WgslBindgenOption::default());
    assert!(matches!(result, Err(CreateModuleError::NonConsecutiveBindGroups)));
  }

  #[test]
  fn create_shader_module_repeated_bindings() {
    let source = indoc! {r#"
            struct A {
                f: vec4<f32>
            };
            @group(0) @binding(2) var<uniform> a: A;
            @group(0) @binding(2) var<uniform> b: A;

            @fragment
            fn main() {}
        "#};

    let result = create_shader_module(source, WgslBindgenOption::default());
    assert!(matches!(result, Err(CreateModuleError::DuplicateBinding { binding: 2 })));
  }

  #[test]
  fn write_vertex_module_empty() {
    let source = indoc! {r#"
            @vertex
            fn main() {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(quote!(), actual);
  }

  #[test]
  fn write_vertex_module_single_input_float32() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: f32,
                @location(1) b: vec2<f32>,
                @location(2) c: vec3<f32>,
                @location(3) d: vec4<f32>,
            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32x2,
                      offset: std::mem::offset_of!(VertexInput0, b) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32x3,
                      offset: std::mem::offset_of!(VertexInput0, c) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32x4,
                      offset: std::mem::offset_of!(VertexInput0, d) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_vertex_module_single_input_float64() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: f64,
                @location(1) b: vec2<f64>,
                @location(2) c: vec3<f64>,
                @location(3) d: vec4<f64>,
            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64x2,
                      offset: std::mem::offset_of!(VertexInput0, b) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64x3,
                      offset: std::mem::offset_of!(VertexInput0, c) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64x4,
                      offset: std::mem::offset_of!(VertexInput0, d) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_vertex_module_single_input_sint32() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: i32,
                @location(1) a: vec2<i32>,
                @location(2) a: vec3<i32>,
                @location(3) a: vec4<i32>,

            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32x2,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32x3,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32x4,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_vertex_module_single_input_uint32() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: u32,
                @location(1) b: vec2<u32>,
                @location(2) c: vec3<u32>,
                @location(3) d: vec4<u32>,
            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32x2,
                      offset: std::mem::offset_of!(VertexInput0, b) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32x3,
                      offset: std::mem::offset_of!(VertexInput0, c) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32x4,
                      offset: std::mem::offset_of!(VertexInput0, d) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_compute_module_empty() {
    let source = indoc! {r#"
            @vertex
            fn main() {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = compute_module(&module);

    assert_tokens_eq!(quote!(), actual);
  }

  #[test]
  fn write_compute_module_multiple_entries() {
    let source = indoc! {r#"
            @compute
            @workgroup_size(1,2,3)
            fn main1() {}

            @compute
            @workgroup_size(256)
            fn main2() {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = compute_module(&module);

    assert_tokens_eq!(
      quote! {
          pub mod compute {
              pub const MAIN1_WORKGROUP_SIZE: [u32; 3] = [1, 2, 3];
              pub fn create_main1_pipeline(device: &wgpu::Device) -> wgpu::ComputePipeline {
                  let module = super::create_shader_module(device);
                  let layout = super::create_pipeline_layout(device);
                  device
                      .create_compute_pipeline(
                          &wgpu::ComputePipelineDescriptor {
                              label: Some("Compute Pipeline main1"),
                              layout: Some(&layout),
                              module: &module,
                              entry_point: "main1",
                          },
                      )
              }
              pub const MAIN2_WORKGROUP_SIZE: [u32; 3] = [256, 1, 1];
              pub fn create_main2_pipeline(device: &wgpu::Device) -> wgpu::ComputePipeline {
                  let module = super::create_shader_module(device);
                  let layout = super::create_pipeline_layout(device);
                  device
                      .create_compute_pipeline(
                          &wgpu::ComputePipelineDescriptor {
                              label: Some("Compute Pipeline main2"),
                              layout: Some(&layout),
                              module: &module,
                              entry_point: "main2",
                          },
                      )
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_entry_constants() {
    let source = indoc! {r#"
            @vertex
            fn vs_main() {}

            @vertex
            fn another_vs() {}

            @fragment
            fn fs_main() {}

            @fragment
            fn another_fs() {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = entry_point_constants(&module);

    assert_tokens_eq!(
      quote! {
          pub const ENTRY_VS_MAIN: &str = "vs_main";
          pub const ENTRY_ANOTHER_VS: &str = "another_vs";
          pub const ENTRY_FS_MAIN: &str = "fs_main";
          pub const ENTRY_ANOTHER_FS: &str = "another_fs";
      },
      actual
    )
  }

  #[test]
  fn write_vertex_shader_entry_no_buffers() {
    let source = indoc! {r#"
            @vertex
            fn vs_main() {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(
      quote! {
          #[derive(Debug)]
          pub struct VertexEntry<const N: usize> {
              entry_point: &'static str,
              buffers: [wgpu::VertexBufferLayout<'static>; N],
          }
          pub fn vertex_state<'a, const N: usize>(
              module: &'a wgpu::ShaderModule,
              entry: &'a VertexEntry<N>,
          ) -> wgpu::VertexState<'a> {
              wgpu::VertexState {
                  module,
                  entry_point: entry.entry_point,
                  buffers: &entry.buffers,
              }
          }
          pub fn vs_main_entry() -> VertexEntry<0> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN,
                  buffers: [],
              }
          }
      },
      actual
    )
  }

  #[test]
  fn write_vertex_shader_multiple_entries() {
    let source = indoc! {r#"
            struct VertexInput {
                @location(0) position: vec4<f32>,
            };
            @vertex
            fn vs_main_1(in: VertexInput) {}

            @vertex
            fn vs_main_2(in: VertexInput) {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(
      quote! {
          #[derive(Debug)]
          pub struct VertexEntry<const N: usize> {
              entry_point: &'static str,
              buffers: [wgpu::VertexBufferLayout<'static>; N],
          }
          pub fn vertex_state<'a, const N: usize>(
              module: &'a wgpu::ShaderModule,
              entry: &'a VertexEntry<N>,
          ) -> wgpu::VertexState<'a> {
              wgpu::VertexState {
                  module,
                  entry_point: entry.entry_point,
                  buffers: &entry.buffers,
              }
          }
          pub fn vs_main_1_entry(vertex_input: wgpu::VertexStepMode) -> VertexEntry<1> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN_1,
                  buffers: [VertexInput::vertex_buffer_layout(vertex_input)],
              }
          }
          pub fn vs_main_2_entry(vertex_input: wgpu::VertexStepMode) -> VertexEntry<1> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN_2,
                  buffers: [VertexInput::vertex_buffer_layout(vertex_input)],
              }
          }
      },
      actual
    )
  }

  #[test]
  fn write_vertex_shader_entry_multiple_buffers() {
    let source = indoc! {r#"
            struct Input0 {
                @location(0) position: vec4<f32>,
            };
            struct Input1 {
                @location(1) some_data: vec2<f32>
            }
            @vertex
            fn vs_main(in0: Input0, in1: Input1) {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(
      quote! {
          #[derive(Debug)]
          pub struct VertexEntry<const N: usize> {
              entry_point: &'static str,
              buffers: [wgpu::VertexBufferLayout<'static>; N],
          }
          pub fn vertex_state<'a, const N: usize>(
              module: &'a wgpu::ShaderModule,
              entry: &'a VertexEntry<N>,
          ) -> wgpu::VertexState<'a> {
              wgpu::VertexState {
                  module,
                  entry_point: entry.entry_point,
                  buffers: &entry.buffers,
              }
          }
          pub fn vs_main_entry(input0: wgpu::VertexStepMode, input1: wgpu::VertexStepMode) -> VertexEntry<2> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN,
                  buffers: [
                      Input0::vertex_buffer_layout(input0),
                      Input1::vertex_buffer_layout(input1),
                  ],
              }
          }
      },
      actual
    )
  }

  #[test]
  fn write_vertex_states_no_entries() {
    let source = indoc! {r#"
            struct Input {
                @location(0) position: vec4<f32>,
            };
            @fragment
            fn main(in: Input) {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(quote!(), actual)
  }
}
