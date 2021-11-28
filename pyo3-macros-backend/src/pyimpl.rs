// Copyright (c) 2017-present PyO3 Project and Contributors

use std::collections::HashSet;

use crate::{
    attributes::{self, take_pyo3_options, PyO3PathAttribute},
    konst::{ConstAttributes, ConstSpec},
    pyfunction::PyFunctionOptions,
    pymethod::{self, is_proto_method},
    utils::get_pyo3_path,
};
use proc_macro2::TokenStream;
use pymethod::GeneratedPyMethod;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    spanned::Spanned,
    Result,
};

/// The mechanism used to collect `#[pymethods]` into the type object
#[derive(Copy, Clone)]
pub enum PyClassMethodsType {
    Specialization,
    Inventory,
}

enum PyImplPyO3Option {
    PyO3Path(PyO3PathAttribute),
}

impl Parse for PyImplPyO3Option {
    fn parse(input: ParseStream) -> Result<Self> {
        let lookahead = input.lookahead1();
        if lookahead.peek(attributes::kw::pyo3_path) {
            input.parse().map(PyImplPyO3Option::PyO3Path)
        } else {
            Err(lookahead.error())
        }
    }
}

#[derive(Default)]
pub struct PyImplOptions {
    pyo3_path: Option<PyO3PathAttribute>,
}

impl PyImplOptions {
    pub fn from_attrs(attrs: &mut Vec<syn::Attribute>) -> Result<Self> {
        let mut options: PyImplOptions = Default::default();

        for option in take_pyo3_options(attrs)? {
            match option {
                PyImplPyO3Option::PyO3Path(path) => options.set_pyo3_path(path)?,
            }
        }

        Ok(options)
    }

    fn set_pyo3_path(&mut self, path: PyO3PathAttribute) -> Result<()> {
        ensure_spanned!(
            self.pyo3_path.is_none(),
            path.0.span() => "`pyo3_path` may only be specified once"
        );

        self.pyo3_path = Some(path);
        Ok(())
    }
}

pub fn build_py_methods(
    ast: &mut syn::ItemImpl,
    methods_type: PyClassMethodsType,
) -> syn::Result<TokenStream> {
    if let Some((_, path, _)) = &ast.trait_ {
        bail_spanned!(path.span() => "#[pymethods] cannot be used on trait impl blocks");
    } else if ast.generics != Default::default() {
        bail_spanned!(
            ast.generics.span() =>
            "#[pymethods] cannot be used with lifetime parameters or generics"
        );
    } else {
        let options = PyImplOptions::from_attrs(&mut ast.attrs)?;
        impl_methods(&ast.self_ty, &mut ast.items, methods_type, options)
    }
}

pub fn impl_methods(
    ty: &syn::Type,
    impls: &mut Vec<syn::ImplItem>,
    methods_type: PyClassMethodsType,
    options: PyImplOptions,
) -> syn::Result<TokenStream> {
    let mut trait_impls = Vec::new();
    let mut proto_impls = Vec::new();
    let mut methods = Vec::new();

    let mut implemented_proto_fragments = HashSet::new();

    for iimpl in impls.iter_mut() {
        match iimpl {
            syn::ImplItem::Method(meth) => {
                let options = PyFunctionOptions::from_attrs(&mut meth.attrs)?;
                match pymethod::gen_py_method(ty, &mut meth.sig, &mut meth.attrs, options)? {
                    GeneratedPyMethod::Method(token_stream) => {
                        let attrs = get_cfg_attributes(&meth.attrs);
                        methods.push(quote!(#(#attrs)* #token_stream));
                    }
                    GeneratedPyMethod::TraitImpl(token_stream) => {
                        let attrs = get_cfg_attributes(&meth.attrs);
                        trait_impls.push(quote!(#(#attrs)* #token_stream));
                    }
                    GeneratedPyMethod::SlotTraitImpl(method_name, token_stream) => {
                        implemented_proto_fragments.insert(method_name);
                        let attrs = get_cfg_attributes(&meth.attrs);
                        trait_impls.push(quote!(#(#attrs)* #token_stream));
                    }
                    GeneratedPyMethod::Proto(token_stream) => {
                        let attrs = get_cfg_attributes(&meth.attrs);
                        proto_impls.push(quote!(#(#attrs)* #token_stream))
                    }
                }
            }
            syn::ImplItem::Const(konst) => {
                let attributes = ConstAttributes::from_attrs(&mut konst.attrs)?;
                if attributes.is_class_attr {
                    let spec = ConstSpec {
                        rust_ident: konst.ident.clone(),
                        attributes,
                    };
                    let attrs = get_cfg_attributes(&konst.attrs);
                    let meth = gen_py_const(ty, &spec);
                    methods.push(quote!(#(#attrs)* #meth));
                    if is_proto_method(&spec.python_name().to_string()) {
                        // If this is a known protocol method e.g. __contains__, then allow this
                        // symbol even though it's not an uppercase constant.
                        konst
                            .attrs
                            .push(syn::parse_quote!(#[allow(non_upper_case_globals)]));
                    }
                }
            }
            _ => (),
        }
    }

    add_shared_proto_slots(ty, &mut proto_impls, implemented_proto_fragments);

    let pyo3_path = get_pyo3_path(&options.pyo3_path);

    Ok(match methods_type {
        PyClassMethodsType::Specialization => {
            let methods_registration = impl_py_methods(ty, methods);
            let protos_registration = impl_protos(ty, proto_impls);

            quote! {
                const _: () = {
                    use #pyo3_path as _pyo3;

                    #(#trait_impls)*

                    #protos_registration

                    #methods_registration
                };
            }
        }
        PyClassMethodsType::Inventory => {
            let inventory = submit_methods_inventory(ty, methods, proto_impls);
            quote! {
                const _: () = {
                    use #pyo3_path as _pyo3;

                    #(#trait_impls)*

                    #inventory
                };
            }
        }
    })
}

pub fn gen_py_const(cls: &syn::Type, spec: &ConstSpec) -> TokenStream {
    let member = &spec.rust_ident;
    let deprecations = &spec.attributes.deprecations;
    let python_name = &spec.null_terminated_python_name();
    quote! {
        _pyo3::class::PyMethodDefType::ClassAttribute({
            _pyo3::class::PyClassAttributeDef::new(
                #python_name,
                _pyo3::class::methods::PyClassAttributeFactory({
                    fn __wrap(py: _pyo3::Python<'_>) -> _pyo3::PyObject {
                        #deprecations
                        _pyo3::IntoPy::into_py(#cls::#member, py)
                    }
                    __wrap
                })
            )
        })
    }
}

fn impl_py_methods(ty: &syn::Type, methods: Vec<TokenStream>) -> TokenStream {
    quote! {
        impl _pyo3::class::impl_::PyMethods<#ty>
            for _pyo3::class::impl_::PyClassImplCollector<#ty>
        {
            fn py_methods(self) -> &'static [_pyo3::class::methods::PyMethodDefType] {
                static METHODS: &[_pyo3::class::methods::PyMethodDefType] = &[#(#methods),*];
                METHODS
            }
        }
    }
}

fn add_shared_proto_slots(
    ty: &syn::Type,
    proto_impls: &mut Vec<TokenStream>,
    mut implemented_proto_fragments: HashSet<String>,
) {
    macro_rules! try_add_shared_slot {
        ($first:literal, $second:literal, $slot:ident) => {{
            let first_implemented = implemented_proto_fragments.remove($first);
            let second_implemented = implemented_proto_fragments.remove($second);
            if first_implemented || second_implemented {
                proto_impls.push(quote! { _pyo3::$slot!(#ty) })
            }
        }};
    }

    try_add_shared_slot!("__setattr__", "__delattr__", generate_pyclass_setattr_slot);
    try_add_shared_slot!("__set__", "__delete__", generate_pyclass_setdescr_slot);
    try_add_shared_slot!("__setitem__", "__delitem__", generate_pyclass_setitem_slot);
    try_add_shared_slot!("__add__", "__radd__", generate_pyclass_add_slot);
    try_add_shared_slot!("__sub__", "__rsub__", generate_pyclass_sub_slot);
    try_add_shared_slot!("__mul__", "__rmul__", generate_pyclass_mul_slot);
    try_add_shared_slot!("__mod__", "__rmod__", generate_pyclass_mod_slot);
    try_add_shared_slot!("__divmod__", "__rdivmod__", generate_pyclass_divmod_slot);
    try_add_shared_slot!("__lshift__", "__rlshift__", generate_pyclass_lshift_slot);
    try_add_shared_slot!("__rshift__", "__rrshift__", generate_pyclass_rshift_slot);
    try_add_shared_slot!("__and__", "__rand__", generate_pyclass_and_slot);
    try_add_shared_slot!("__or__", "__ror__", generate_pyclass_or_slot);
    try_add_shared_slot!("__xor__", "__rxor__", generate_pyclass_xor_slot);
    try_add_shared_slot!("__matmul__", "__rmatmul__", generate_pyclass_matmul_slot);
    try_add_shared_slot!("__truediv__", "__rtruediv__", generate_pyclass_truediv_slot);
    try_add_shared_slot!(
        "__floordiv__",
        "__rfloordiv__",
        generate_pyclass_floordiv_slot
    );
    try_add_shared_slot!("__pow__", "__rpow__", generate_pyclass_pow_slot);

    assert!(implemented_proto_fragments.is_empty());
}

fn impl_protos(ty: &syn::Type, proto_impls: Vec<TokenStream>) -> TokenStream {
    quote! {
        impl _pyo3::class::impl_::PyMethodsProtocolSlots<#ty>
            for _pyo3::class::impl_::PyClassImplCollector<#ty>
        {
            fn methods_protocol_slots(self) -> &'static [_pyo3::ffi::PyType_Slot] {
                &[#(#proto_impls),*]
            }
        }
    }
}

fn submit_methods_inventory(
    ty: &syn::Type,
    methods: Vec<TokenStream>,
    proto_impls: Vec<TokenStream>,
) -> TokenStream {
    quote! {
        _pyo3::inventory::submit! {
            type Inventory = <#ty as _pyo3::class::impl_::PyClassImpl>::Inventory;
            Inventory::new(&[#(#methods),*], &[#(#proto_impls),*])
        }
    }
}

fn get_cfg_attributes(attrs: &[syn::Attribute]) -> Vec<&syn::Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path.is_ident("cfg"))
        .collect()
}
