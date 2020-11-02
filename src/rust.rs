use crate::shell::Shell;
use anyhow::{anyhow, bail, Context as _};
use cargo_metadata as cm;
use fixedbitset::FixedBitSet;
use if_chain::if_chain;
use itertools::Itertools as _;
use maplit::{btreemap, btreeset, hashset};
use proc_macro2::{LineColumn, Spacing, Span, TokenStream, TokenTree};
use quote::{quote, ToTokens};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    mem,
    ops::{Add, AddAssign},
    str,
};
use syn::{
    parse_quote,
    spanned::Spanned,
    visit::{self, Visit},
    Arm, Attribute, BareFnArg, ConstParam, ExprArray, ExprAssign, ExprAssignOp, ExprAsync,
    ExprAwait, ExprBinary, ExprBlock, ExprBox, ExprBreak, ExprCall, ExprCast, ExprClosure,
    ExprContinue, ExprField, ExprForLoop, ExprGroup, ExprIf, ExprIndex, ExprLet, ExprLit, ExprLoop,
    ExprMacro, ExprMatch, ExprMethodCall, ExprParen, ExprPath, ExprRange, ExprReference,
    ExprRepeat, ExprReturn, ExprStruct, ExprTry, ExprTryBlock, ExprTuple, ExprType, ExprUnary,
    ExprUnsafe, ExprWhile, ExprYield, Field, FieldPat, FieldValue, ForeignItemFn, ForeignItemMacro,
    ForeignItemStatic, ForeignItemType, Ident, ImplItemConst, ImplItemMacro, ImplItemMethod,
    ImplItemType, Item, ItemConst, ItemEnum, ItemExternCrate, ItemFn, ItemForeignMod, ItemImpl,
    ItemMacro, ItemMacro2, ItemMod, ItemStatic, ItemStruct, ItemTrait, ItemTraitAlias, ItemType,
    ItemUnion, ItemUse, LifetimeDef, Lit, Local, Macro, Meta, MetaList, MetaNameValue, NestedMeta,
    PatBox, PatIdent, PatLit, PatMacro, PatOr, PatPath, PatRange, PatReference, PatRest, PatSlice,
    PatStruct, PatTuple, PatTupleStruct, PatType, PatWild, PathSegment, Receiver, TraitItemConst,
    TraitItemMacro, TraitItemMethod, TraitItemType, TypeParam, UseGroup, UseName, UsePath,
    UseRename, UseTree, Variadic, Variant, VisRestricted,
};

pub(crate) fn find_uses(code: &str) -> anyhow::Result<Option<(String, Vec<ItemUse>)>> {
    let syn::File { attrs, items, .. } = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    if let Some(span) = attrs.iter().find_map(|attr| {
        let span = attr.span();
        if_chain! {
            if let Ok(meta) = attr.parse_meta();
            if let Meta::List(MetaList { path, nested, .. }) = &meta;
            if matches!(path.get_ident(), Some(i) if i == "cfg_attr");
            if let [expr, attrs @ ..] = &*nested.iter().collect::<Vec<_>>();
            let expr = expr.to_token_stream().to_string();
            if let Ok(expr) = cfg_expr::Expression::parse(&expr);
            if expr.eval(|pred| *pred == cfg_expr::Predicate::Flag("cargo_equip"));
            then {
                for attr in attrs {
                    if_chain! {
                        if let NestedMeta::Meta(attr) = attr;
                        if let [seg1, seg2] = *attr.path().segments.iter().collect::<Vec<_>>();
                        if matches!(seg1, PathSegment { ident, .. } if ident == "cargo_equip");
                        if let PathSegment { ident, .. } = seg2;
                        if ident == "equip";
                        then {
                            return Some(span);
                        }
                    }
                }
            }
        }
        None
    }) {
        let mut replacements = btreemap!();
        let mut uses = vec![];

        let mut comment_out = |span: Span| {
            replacements.insert((span.start(), span.start()), "/*".to_owned());
            replacements.insert((span.end(), span.end()), "*/".to_owned());
        };

        comment_out(span);

        for item in items {
            let span = item.span();
            if let Item::Use(item_use) = item {
                if item_use.leading_colon.is_some() {
                    comment_out(span);
                    uses.push(item_use);
                }
            }
        }

        Ok(Some((replace_ranges(code, replacements), uses)))
    } else {
        Ok(None)
    }
}

pub(crate) enum ModNames {
    Scoped(HashSet<String>),
    All,
}

impl ModNames {
    fn extract_from_depth_2(tree: &UseTree) -> Self {
        match tree {
            UseTree::Path(UsePath { ident, .. })
            | UseTree::Name(UseName { ident })
            | UseTree::Rename(UseRename { ident, .. }) => Self::Scoped(hashset!(ident.to_string())),
            UseTree::Group(UseGroup { items, .. }) => items
                .iter()
                .map(Self::extract_from_depth_2)
                .fold(Self::Scoped(hashset!()), Add::add),
            UseTree::Glob(_) => Self::All,
        }
    }
}

impl Default for ModNames {
    fn default() -> Self {
        Self::Scoped(hashset!())
    }
}

impl Add for ModNames {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        match (self, rhs) {
            (Self::Scoped(l), Self::Scoped(r)) => Self::Scoped(l.into_iter().chain(r).collect()),
            (Self::All, _) | (_, Self::All) => Self::All,
        }
    }
}

impl AddAssign for ModNames {
    fn add_assign(&mut self, rhs: Self) {
        *self = mem::take(self) + rhs;
    }
}

pub(crate) fn extract_names(uses: &[ItemUse]) -> BTreeMap<&Ident, ModNames> {
    let mut mod_names = btreemap!();

    for tree in uses
        .iter()
        .filter(|ItemUse { leading_colon, .. }| leading_colon.is_some())
        .flat_map(|ItemUse { tree, .. }| {
            fn corrupt_groups(tree: &UseTree) -> Vec<&UseTree> {
                if let UseTree::Group(UseGroup { items, .. }) = tree {
                    items.iter().flat_map(corrupt_groups).collect()
                } else {
                    vec![tree]
                }
            }
            corrupt_groups(tree)
        })
    {
        match tree {
            UseTree::Path(UsePath { ident, tree, .. }) => {
                *mod_names.entry(ident).or_default() += ModNames::extract_from_depth_2(tree);
            }
            UseTree::Name(UseName { ident }) | UseTree::Rename(UseRename { ident, .. }) => {
                *mod_names.entry(ident).or_default() = ModNames::All;
            }
            UseTree::Glob(_) => todo!("`use ::*;` is not yet supported"),
            UseTree::Group(_) => unreachable!("should be corrupted here"),
        }
    }

    mod_names
}

pub(crate) fn comment_out_macro_uses(
    code: &str,
    mut is_directly_available_crate: impl FnMut(&str) -> bool,
) -> anyhow::Result<String> {
    let syn::File { items, .. } = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    let mut replacements = btreemap!();

    for item in items {
        if let Item::ExternCrate(ItemExternCrate {
            attrs,
            ident,
            rename: Some((_, rename)),
            ..
        }) = &item
        {
            if attrs
                .iter()
                .flat_map(Attribute::parse_meta)
                .any(|m| matches!(m, Meta::Path(p) if p.is_ident("macro_use")))
                && rename == "_"
                && !is_directly_available_crate(&ident.to_string())
            {
                let pos = item.span().start();
                replacements.insert((pos, pos), "/*".to_owned());
                let pos = item.span().end();
                replacements.insert((pos, pos), "*/".to_owned());
            }
        }
    }

    Ok(replace_ranges(code, replacements))
}

pub(crate) fn expand_mods(src_path: &std::path::Path) -> anyhow::Result<String> {
    fn expand_mods(src_path: &std::path::Path, depth: usize) -> anyhow::Result<String> {
        let content = std::fs::read_to_string(src_path)
            .with_context(|| format!("could not read `{}`", src_path.display()))?;

        let syn::File { items, .. } = syn::parse_file(&content)
            .map_err(|e| anyhow!("{:?}", e))
            .with_context(|| format!("could not parse `{}`", src_path.display()))?;

        let replacements = items
            .into_iter()
            .flat_map(|item| match item {
                Item::Mod(ItemMod {
                    attrs,
                    ident,
                    content: None,
                    semi,
                    ..
                }) => Some((attrs, ident, semi)),
                _ => None,
            })
            .map(|(attrs, ident, semi)| {
                let paths = if let Some(path) = attrs
                    .iter()
                    .flat_map(Attribute::parse_meta)
                    .flat_map(|meta| match meta {
                        Meta::NameValue(name_value) => Some(name_value),
                        _ => None,
                    })
                    .filter(|MetaNameValue { path, .. }| {
                        matches!(path.get_ident(), Some(i) if i == "path")
                    })
                    .find_map(|MetaNameValue { lit, .. }| match lit {
                        Lit::Str(s) => Some(s.value()),
                        _ => None,
                    }) {
                    vec![src_path.with_file_name("").join(path)]
                } else if depth == 0 || src_path.file_name() == Some("mod.rs".as_ref()) {
                    vec![
                        src_path
                            .with_file_name(&ident.to_string())
                            .with_extension("rs"),
                        src_path.with_file_name(&ident.to_string()).join("mod.rs"),
                    ]
                } else {
                    vec![
                        src_path
                            .with_extension("")
                            .with_file_name(&ident.to_string())
                            .with_extension("rs"),
                        src_path
                            .with_extension("")
                            .with_file_name(&ident.to_string())
                            .join("mod.rs"),
                    ]
                };

                if let Some(path) = paths.iter().find(|p| p.exists()) {
                    let start = semi.span().start();
                    let end = semi.span().end();
                    let content = expand_mods(&path, depth + 1)?;
                    let content = indent_code(&content, depth + 1);
                    let content = format!(" {{\n{}{}}}", content, "    ".repeat(depth + 1));
                    Ok(((start, end), content))
                } else {
                    bail!("one of {:?} does not exist", paths);
                }
            })
            .collect::<anyhow::Result<_>>()?;

        Ok(replace_ranges(&content, replacements))
    }

    expand_mods(src_path, 0)
}

pub(crate) fn indent_code(code: &str, n: usize) -> String {
    let is_safe_to_indent = code.parse::<TokenStream>().map_or(false, |token_stream| {
        !token_stream.into_iter().any(|tt| {
            matches!(
                tt, TokenTree::Literal(lit)
                if lit.span().start().line != lit.span().end().line
            )
        })
    });

    if is_safe_to_indent {
        code.lines()
            .map(|line| match line {
                "" => "\n".to_owned(),
                line => format!("{}{}\n", "    ".repeat(n), line),
            })
            .join("")
    } else {
        code.to_owned()
    }
}

pub(crate) fn remove_toplevel_items_except_mods_and_extern_crates(
    code: &str,
) -> anyhow::Result<String> {
    let syn::File { items, .. } = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    let mut replacements = btreemap!();

    for item in items {
        if !matches!(&item, Item::Mod(_) | Item::ExternCrate(_)) {
            replacements.insert((item.span().start(), item.span().end()), "".to_owned());
        }
    }

    Ok(replace_ranges(code, replacements))
}

pub(crate) fn replace_crate_paths(
    code: &str,
    extern_crate_name: &str,
    shell: &mut Shell,
) -> anyhow::Result<String> {
    struct Visitor<'a> {
        extern_crate_name: &'a str,
        replacements: BTreeMap<(LineColumn, LineColumn), String>,
    }

    impl Visitor<'_> {
        fn insert(&mut self, crate_token: &Ident) {
            let pos = crate_token.span().end();
            self.replacements
                .insert((pos, pos), format!("::{}", self.extern_crate_name));
        }
    }

    impl Visit<'_> for Visitor<'_> {
        fn visit_path(&mut self, path: &'_ syn::Path) {
            if let (None, Some(first)) = (path.leading_colon, path.segments.first()) {
                if first.ident == "crate" && first.arguments.is_empty() {
                    self.insert(&first.ident);
                }
            }
        }

        fn visit_item_use(&mut self, item_use: &'_ ItemUse) {
            if item_use.leading_colon.is_none() {
                self.visit_use_tree(&item_use.tree);
            }
        }

        fn visit_use_tree(&mut self, use_tree: &UseTree) {
            match &use_tree {
                UseTree::Path(UsePath { ident, .. })
                | UseTree::Name(UseName { ident })
                | UseTree::Rename(UseRename { ident, .. })
                    if ident == "crate" =>
                {
                    self.insert(ident);
                }
                UseTree::Group(UseGroup { items, .. }) => {
                    for item in items {
                        self.visit_use_tree(item);
                    }
                }
                _ => {}
            }
        }

        fn visit_vis_restricted(&mut self, vis_restricted: &VisRestricted) {
            if vis_restricted.in_token.is_some() {
                self.visit_path(&vis_restricted.path);
            } else if let Some(ident) = vis_restricted.path.get_ident() {
                if ident == "crate" {
                    let pos = vis_restricted.path.span().start();
                    self.replacements.insert((pos, pos), "in ".to_owned());

                    self.insert(ident);
                }
            }
        }
    }

    let file = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    let mut visitor = Visitor {
        extern_crate_name,
        replacements: btreemap!(),
    };

    visitor.visit_file(&file);

    let Visitor { replacements, .. } = visitor;

    if replacements.is_empty() {
        Ok(code.to_owned())
    } else {
        shell.warn(format!(
            "found `crate` paths. replacing them with `crate::{}`",
            extern_crate_name,
        ))?;
        Ok(replace_ranges(code, replacements))
    }
}

pub(crate) fn list_mod_names(code: &str) -> anyhow::Result<HashSet<String>> {
    let syn::File { items, .. } = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    Ok(items
        .into_iter()
        .flat_map(|item| match item {
            Item::Mod(ItemMod { ident, .. }) => Some(ident.to_string()),
            _ => None,
        })
        .collect())
}

pub(crate) struct LibContent<'cm> {
    pub(crate) package_id: &'cm cm::PackageId,
    pub(crate) extern_crate_name: Ident,
    pub(crate) content: String,
    pub(crate) modules: HashSet<String>,
}

pub(crate) fn shift_use_statements(uses: &[ItemUse]) -> Vec<ItemUse> {
    uses.iter()
        .filter(|ItemUse { leading_colon, .. }| leading_colon.is_some())
        .flat_map(|ItemUse { tree, .. }| {
            fn corrupt_groups(tree: &UseTree) -> Vec<&UseTree> {
                if let UseTree::Group(UseGroup { items, .. }) = tree {
                    items.iter().flat_map(corrupt_groups).collect()
                } else {
                    vec![tree]
                }
            }
            corrupt_groups(tree)
        })
        .map(|tree| parse_quote!(use crate::#tree;))
        .collect()
}

pub(crate) fn remove_unused_modules(
    contents: &mut [LibContent<'_>],
    used: Option<&BTreeSet<(&cm::PackageId, String)>>,
) -> anyhow::Result<()> {
    for LibContent {
        package_id,
        content,
        modules,
        ..
    } in contents
    {
        let syn::File { items, .. } = syn::parse_file(content)
            .map_err(|e| anyhow!("{:?}", e))
            .with_context(|| "could not parse the code")?;

        let mut replacements = btreemap!();

        for item in items {
            if let Item::Mod(ItemMod { ident, .. }) = &item {
                if_chain! {
                    if let Some(used) = used;
                    if !used.contains(&(*package_id, ident.to_string()));
                    then {
                        replacements
                            .insert((item.span().start(), item.span().end()), "".to_owned());
                    } else {
                        modules.insert(ident.to_string());
                    }
                }
            }
        }

        *content = replace_ranges(content, replacements);
    }

    Ok(())
}

pub(crate) fn replace_extern_crates(
    code: &str,
    convert_extern_crate_name: impl FnMut(&syn::Ident) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    struct Visitor<'a, F> {
        replacements: &'a mut anyhow::Result<BTreeMap<(LineColumn, LineColumn), String>>,
        convert_extern_crate_name: F,
    };

    impl<F: FnMut(&syn::Ident) -> anyhow::Result<String>> Visit<'_> for Visitor<'_, F> {
        fn visit_item_extern_crate(&mut self, item_use: &ItemExternCrate) {
            let ItemExternCrate {
                attrs,
                vis,
                ident,
                rename,
                semi_token,
                ..
            } = item_use;

            let to = match (self.convert_extern_crate_name)(ident) {
                Ok(to) => Ident::new(&to, Span::call_site()),
                Err(err) => {
                    *self.replacements = Err(err);
                    return;
                }
            };
            if let Ok(replacements) = &mut self.replacements {
                replacements.insert(
                    (item_use.span().start(), semi_token.span().end()),
                    if let Some((_, rename)) = rename {
                        quote!(#(#attrs)* #vis use crate::#to as #rename;).to_string()
                    } else {
                        quote!(#(#attrs)* #vis use crate::#to as #ident;).to_string()
                    },
                );
            }
        }
    }

    let file = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    let mut replacements = Ok(btreemap!());

    Visitor {
        replacements: &mut replacements,
        convert_extern_crate_name,
    }
    .visit_file(&file);

    let replacements = replacements?;

    Ok(replace_ranges(code, replacements))
}

pub(crate) fn modify_macros(code: &str, extern_crate_name: &str) -> anyhow::Result<String> {
    fn find_dollar_crates(token_stream: TokenStream, acc: &mut BTreeSet<LineColumn>) {
        for (i, (tt1, tt2)) in token_stream.into_iter().tuple_windows().enumerate() {
            if i == 0 {
                if let proc_macro2::TokenTree::Group(group) = &tt1 {
                    find_dollar_crates(group.stream(), acc);
                }
            }

            if let proc_macro2::TokenTree::Group(group) = &tt2 {
                find_dollar_crates(group.stream(), acc);
            }

            if matches!(
                (&tt1, &tt2),
                (proc_macro2::TokenTree::Punct(p), proc_macro2::TokenTree::Ident(i))
                if p.as_char() == '$' && i == "crate"
            ) {
                acc.insert(tt2.span().end());
            }
        }
    };

    fn exclude_crate_macros(token_stream: TokenStream, acc: &mut BTreeSet<LineColumn>) {
        for tts in token_stream
            .clone()
            .into_iter()
            .collect::<Vec<_>>()
            .windows(6)
        {
            if let [proc_macro2::TokenTree::Punct(punct1), proc_macro2::TokenTree::Ident(ident), proc_macro2::TokenTree::Punct(punct2), proc_macro2::TokenTree::Punct(punct3), proc_macro2::TokenTree::Ident(_), proc_macro2::TokenTree::Punct(punct4)] =
                &*tts
            {
                if punct1.as_char() == '$'
                    && ident == "crate"
                    && punct2.as_char() == ':'
                    && punct3.as_char() == ':'
                    && punct4.as_char() == '!'
                {
                    acc.remove(&ident.span().end());
                }
            }
        }

        for tt in token_stream.clone() {
            if let proc_macro2::TokenTree::Group(group) = tt {
                exclude_crate_macros(group.stream(), acc);
            }
        }
    }

    struct Visitor<'a> {
        dollar_crates: &'a mut BTreeSet<LineColumn>,
    }

    impl Visit<'_> for Visitor<'_> {
        fn visit_item_macro(&mut self, i: &ItemMacro) {
            if let ItemMacro {
                ident: Some(_),
                mac: Macro { tokens, .. },
                ..
            } = i
            {
                find_dollar_crates(tokens.clone(), &mut self.dollar_crates);
                exclude_crate_macros(tokens.clone(), &mut self.dollar_crates);
            }
        }
    }

    let file = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    let mut dollar_crates = btreeset!();

    Visitor {
        dollar_crates: &mut dollar_crates,
    }
    .visit_file(&file);

    Ok(replace_ranges(
        code,
        dollar_crates
            .into_iter()
            .map(|p| ((p, p), format!("::{}", extern_crate_name)))
            .collect(),
    ))
}

fn replace_ranges(code: &str, replacements: BTreeMap<(LineColumn, LineColumn), String>) -> String {
    let replacements = replacements.into_iter().collect::<Vec<_>>();
    let mut replacements = &*replacements;
    let mut skip_until = None;
    let mut ret = "".to_owned();
    let mut lines = code.trim_end().split('\n').enumerate().peekable();
    while let Some((i, s)) = lines.next() {
        for (j, c) in s.chars().enumerate() {
            if_chain! {
                if let Some(((start, end), replacement)) = replacements.get(0);
                if (i, j) == (start.line - 1, start.column);
                then {
                    ret += replacement;
                    if start == end {
                        ret.push(c);
                    } else {
                        skip_until = Some(*end);
                    }
                    replacements = &replacements[1..];
                } else {
                    if !matches!(skip_until, Some(LineColumn { line, column }) if (i, j) < (line - 1, column)) {
                        ret.push(c);
                        skip_until = None;
                    }
                }
            }
        }
        while let Some(((start, end), replacement)) = replacements.get(0) {
            if i == start.line - 1 {
                ret += replacement;
                if start < end {
                    skip_until = Some(*end);
                }
                replacements = &replacements[1..];
            } else {
                break;
            }
        }
        if lines.peek().is_some() || code.ends_with('\n') {
            ret += "\n";
        }
    }

    debug_assert!(syn::parse_file(&code).is_ok());

    ret
}

pub(crate) fn prepend_mod_doc(code: &str, append: &str) -> syn::Result<String> {
    let syn::File { shebang, attrs, .. } = syn::parse_file(code)?;

    let mut code = code.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let mut doc = vec![];

    if shebang.is_some() {
        code[0] = "".to_owned();
    }

    for (val, span) in attrs
        .iter()
        .flat_map(Attribute::parse_meta)
        .flat_map(|meta| match meta {
            Meta::NameValue(name_value) => Some(name_value),
            _ => None,
        })
        .filter(|MetaNameValue { path, .. }| matches!(path.get_ident(), Some(i) if i == "doc"))
        .flat_map(|name_value| match &name_value.lit {
            Lit::Str(val) => Some((val.value(), name_value.span())),
            _ => None,
        })
    {
        doc.push(val);

        if span.start().line == span.end().line {
            let i = span.start().line - 1;
            let l = span.start().column;
            let r = span.end().column;
            code[i] = format!("{}{}{}", &code[i][..l], " ".repeat(r - l), &code[i][r..]);
        } else {
            let i = span.start().line - 1;
            let l = span.start().column;
            code[i] = format!("{}{}", &code[i][..l], code[i].len() - l);

            for line in &mut code[span.start().line..span.end().line - 2] {
                *line = " ".repeat(line.len());
            }

            let i = span.end().line - 1;
            let r = span.end().column;
            code[i] = format!("{}{}", " ".repeat(code[i].len() - r), &code[i][r..]);
        }
    }

    Ok(format!(
        "{}{}{}{}\n{}\n",
        match shebang {
            Some(shebang) => format!("{}\n", shebang),
            None => "".to_owned(),
        },
        doc.iter()
            .format_with("", |l, f| f(&format_args!("//!{}\n", l))),
        if doc.iter().all(|s| s.is_empty()) {
            ""
        } else {
            "//!\n"
        },
        append
            .lines()
            .format_with("", |l, f| f(&format_args!("//!{}\n", l))),
        code.join("\n").trim_start(),
    ))
}

pub(crate) fn resolve_cfgs(code: &str, features: &[String]) -> anyhow::Result<String> {
    struct Visitor<'a> {
        replacements: &'a mut BTreeMap<(LineColumn, LineColumn), String>,
        features: &'a [String],
    }

    impl Visitor<'_> {
        fn proceed<'a, T: ToTokens>(
            &mut self,
            i: &'a T,
            attrs: fn(&T) -> &[Attribute],
            visit: fn(&mut Self, &'a T),
        ) {
            let sufficiencies = attrs(i)
                .iter()
                .flat_map(|a| a.parse_meta().map(|m| (a.span(), m)))
                .flat_map(|(span, meta)| match meta {
                    Meta::List(meta_list) => Some((span, meta_list)),
                    _ => None,
                })
                .filter(|(_, MetaList { path, .. })| path.is_ident("cfg"))
                .flat_map(|(span, MetaList { nested, .. })| {
                    let expr =
                        cfg_expr::Expression::parse(&nested.to_token_stream().to_string()).ok()?;
                    Some((span, expr))
                })
                .map(|(span, expr)| {
                    let sufficiency = expr.eval(|pred| match pred {
                        cfg_expr::Predicate::Test | cfg_expr::Predicate::ProcMacro => Some(false),
                        cfg_expr::Predicate::Flag("cargo_equip") => Some(true),
                        cfg_expr::Predicate::Feature(feature) => {
                            Some(self.features.contains(&(*feature).to_owned()))
                        }
                        _ => None,
                    });
                    (span, sufficiency)
                })
                .collect::<Vec<_>>();

            if sufficiencies.iter().any(|&(_, p)| p == Some(false)) {
                self.replacements
                    .insert((i.span().start(), i.span().end()), "".to_owned());
            } else {
                for (span, p) in sufficiencies {
                    if p == Some(true) {
                        self.replacements
                            .insert((span.start(), span.end()), "".to_owned());
                    }
                }
                visit(self, i);
            }
        }
    }

    macro_rules! impl_visits {
        ($(fn $method:ident(&mut self, _: &'_ $ty:path) { _(_, _, $visit:path) })*) => {
            $(
                fn $method(&mut self, i: &'_ $ty) {
                    self.proceed(i, |$ty { attrs, .. }| attrs, $visit);
                }
            )*
        };
    }

    impl Visit<'_> for Visitor<'_> {
        impl_visits! {
            fn visit_arm                (&mut self, _: &'_ Arm              ) { _(_, _, visit::visit_arm                ) }
            fn visit_bare_fn_arg        (&mut self, _: &'_ BareFnArg        ) { _(_, _, visit::visit_bare_fn_arg        ) }
            fn visit_const_param        (&mut self, _: &'_ ConstParam       ) { _(_, _, visit::visit_const_param        ) }
            fn visit_expr_array         (&mut self, _: &'_ ExprArray        ) { _(_, _, visit::visit_expr_array         ) }
            fn visit_expr_assign        (&mut self, _: &'_ ExprAssign       ) { _(_, _, visit::visit_expr_assign        ) }
            fn visit_expr_assign_op     (&mut self, _: &'_ ExprAssignOp     ) { _(_, _, visit::visit_expr_assign_op     ) }
            fn visit_expr_async         (&mut self, _: &'_ ExprAsync        ) { _(_, _, visit::visit_expr_async         ) }
            fn visit_expr_await         (&mut self, _: &'_ ExprAwait        ) { _(_, _, visit::visit_expr_await         ) }
            fn visit_expr_binary        (&mut self, _: &'_ ExprBinary       ) { _(_, _, visit::visit_expr_binary        ) }
            fn visit_expr_block         (&mut self, _: &'_ ExprBlock        ) { _(_, _, visit::visit_expr_block         ) }
            fn visit_expr_box           (&mut self, _: &'_ ExprBox          ) { _(_, _, visit::visit_expr_box           ) }
            fn visit_expr_break         (&mut self, _: &'_ ExprBreak        ) { _(_, _, visit::visit_expr_break         ) }
            fn visit_expr_call          (&mut self, _: &'_ ExprCall         ) { _(_, _, visit::visit_expr_call          ) }
            fn visit_expr_cast          (&mut self, _: &'_ ExprCast         ) { _(_, _, visit::visit_expr_cast          ) }
            fn visit_expr_closure       (&mut self, _: &'_ ExprClosure      ) { _(_, _, visit::visit_expr_closure       ) }
            fn visit_expr_continue      (&mut self, _: &'_ ExprContinue     ) { _(_, _, visit::visit_expr_continue      ) }
            fn visit_expr_field         (&mut self, _: &'_ ExprField        ) { _(_, _, visit::visit_expr_field         ) }
            fn visit_expr_for_loop      (&mut self, _: &'_ ExprForLoop      ) { _(_, _, visit::visit_expr_for_loop      ) }
            fn visit_expr_group         (&mut self, _: &'_ ExprGroup        ) { _(_, _, visit::visit_expr_group         ) }
            fn visit_expr_if            (&mut self, _: &'_ ExprIf           ) { _(_, _, visit::visit_expr_if            ) }
            fn visit_expr_index         (&mut self, _: &'_ ExprIndex        ) { _(_, _, visit::visit_expr_index         ) }
            fn visit_expr_let           (&mut self, _: &'_ ExprLet          ) { _(_, _, visit::visit_expr_let           ) }
            fn visit_expr_lit           (&mut self, _: &'_ ExprLit          ) { _(_, _, visit::visit_expr_lit           ) }
            fn visit_expr_loop          (&mut self, _: &'_ ExprLoop         ) { _(_, _, visit::visit_expr_loop          ) }
            fn visit_expr_macro         (&mut self, _: &'_ ExprMacro        ) { _(_, _, visit::visit_expr_macro         ) }
            fn visit_expr_match         (&mut self, _: &'_ ExprMatch        ) { _(_, _, visit::visit_expr_match         ) }
            fn visit_expr_method_call   (&mut self, _: &'_ ExprMethodCall   ) { _(_, _, visit::visit_expr_method_call   ) }
            fn visit_expr_paren         (&mut self, _: &'_ ExprParen        ) { _(_, _, visit::visit_expr_paren         ) }
            fn visit_expr_path          (&mut self, _: &'_ ExprPath         ) { _(_, _, visit::visit_expr_path          ) }
            fn visit_expr_range         (&mut self, _: &'_ ExprRange        ) { _(_, _, visit::visit_expr_range         ) }
            fn visit_expr_reference     (&mut self, _: &'_ ExprReference    ) { _(_, _, visit::visit_expr_reference     ) }
            fn visit_expr_repeat        (&mut self, _: &'_ ExprRepeat       ) { _(_, _, visit::visit_expr_repeat        ) }
            fn visit_expr_return        (&mut self, _: &'_ ExprReturn       ) { _(_, _, visit::visit_expr_return        ) }
            fn visit_expr_struct        (&mut self, _: &'_ ExprStruct       ) { _(_, _, visit::visit_expr_struct        ) }
            fn visit_expr_try           (&mut self, _: &'_ ExprTry          ) { _(_, _, visit::visit_expr_try           ) }
            fn visit_expr_try_block     (&mut self, _: &'_ ExprTryBlock     ) { _(_, _, visit::visit_expr_try_block     ) }
            fn visit_expr_tuple         (&mut self, _: &'_ ExprTuple        ) { _(_, _, visit::visit_expr_tuple         ) }
            fn visit_expr_type          (&mut self, _: &'_ ExprType         ) { _(_, _, visit::visit_expr_type          ) }
            fn visit_expr_unary         (&mut self, _: &'_ ExprUnary        ) { _(_, _, visit::visit_expr_unary         ) }
            fn visit_expr_unsafe        (&mut self, _: &'_ ExprUnsafe       ) { _(_, _, visit::visit_expr_unsafe        ) }
            fn visit_expr_while         (&mut self, _: &'_ ExprWhile        ) { _(_, _, visit::visit_expr_while         ) }
            fn visit_expr_yield         (&mut self, _: &'_ ExprYield        ) { _(_, _, visit::visit_expr_yield         ) }
            fn visit_field              (&mut self, _: &'_ Field            ) { _(_, _, visit::visit_field              ) }
            fn visit_field_pat          (&mut self, _: &'_ FieldPat         ) { _(_, _, visit::visit_field_pat          ) }
            fn visit_field_value        (&mut self, _: &'_ FieldValue       ) { _(_, _, visit::visit_field_value        ) }
            fn visit_file               (&mut self, _: &'_ syn::File        ) { _(_, _, visit::visit_file               ) }
            fn visit_foreign_item_fn    (&mut self, _: &'_ ForeignItemFn    ) { _(_, _, visit::visit_foreign_item_fn    ) }
            fn visit_foreign_item_macro (&mut self, _: &'_ ForeignItemMacro ) { _(_, _, visit::visit_foreign_item_macro ) }
            fn visit_foreign_item_static(&mut self, _: &'_ ForeignItemStatic) { _(_, _, visit::visit_foreign_item_static) }
            fn visit_foreign_item_type  (&mut self, _: &'_ ForeignItemType  ) { _(_, _, visit::visit_foreign_item_type  ) }
            fn visit_impl_item_const    (&mut self, _: &'_ ImplItemConst    ) { _(_, _, visit::visit_impl_item_const    ) }
            fn visit_impl_item_macro    (&mut self, _: &'_ ImplItemMacro    ) { _(_, _, visit::visit_impl_item_macro    ) }
            fn visit_impl_item_method   (&mut self, _: &'_ ImplItemMethod   ) { _(_, _, visit::visit_impl_item_method   ) }
            fn visit_impl_item_type     (&mut self, _: &'_ ImplItemType     ) { _(_, _, visit::visit_impl_item_type     ) }
            fn visit_item_const         (&mut self, _: &'_ ItemConst        ) { _(_, _, visit::visit_item_const         ) }
            fn visit_item_enum          (&mut self, _: &'_ ItemEnum         ) { _(_, _, visit::visit_item_enum          ) }
            fn visit_item_extern_crate  (&mut self, _: &'_ ItemExternCrate  ) { _(_, _, visit::visit_item_extern_crate  ) }
            fn visit_item_fn            (&mut self, _: &'_ ItemFn           ) { _(_, _, visit::visit_item_fn            ) }
            fn visit_item_foreign_mod   (&mut self, _: &'_ ItemForeignMod   ) { _(_, _, visit::visit_item_foreign_mod   ) }
            fn visit_item_impl          (&mut self, _: &'_ ItemImpl         ) { _(_, _, visit::visit_item_impl          ) }
            fn visit_item_macro         (&mut self, _: &'_ ItemMacro        ) { _(_, _, visit::visit_item_macro         ) }
            fn visit_item_macro2        (&mut self, _: &'_ ItemMacro2       ) { _(_, _, visit::visit_item_macro2        ) }
            fn visit_item_mod           (&mut self, _: &'_ ItemMod          ) { _(_, _, visit::visit_item_mod           ) }
            fn visit_item_static        (&mut self, _: &'_ ItemStatic       ) { _(_, _, visit::visit_item_static        ) }
            fn visit_item_struct        (&mut self, _: &'_ ItemStruct       ) { _(_, _, visit::visit_item_struct        ) }
            fn visit_item_trait         (&mut self, _: &'_ ItemTrait        ) { _(_, _, visit::visit_item_trait         ) }
            fn visit_item_trait_alias   (&mut self, _: &'_ ItemTraitAlias   ) { _(_, _, visit::visit_item_trait_alias   ) }
            fn visit_item_type          (&mut self, _: &'_ ItemType         ) { _(_, _, visit::visit_item_type          ) }
            fn visit_item_union         (&mut self, _: &'_ ItemUnion        ) { _(_, _, visit::visit_item_union         ) }
            fn visit_item_use           (&mut self, _: &'_ ItemUse          ) { _(_, _, visit::visit_item_use           ) }
            fn visit_lifetime_def       (&mut self, _: &'_ LifetimeDef      ) { _(_, _, visit::visit_lifetime_def       ) }
            fn visit_local              (&mut self, _: &'_ Local            ) { _(_, _, visit::visit_local              ) }
            fn visit_pat_box            (&mut self, _: &'_ PatBox           ) { _(_, _, visit::visit_pat_box            ) }
            fn visit_pat_ident          (&mut self, _: &'_ PatIdent         ) { _(_, _, visit::visit_pat_ident          ) }
            fn visit_pat_lit            (&mut self, _: &'_ PatLit           ) { _(_, _, visit::visit_pat_lit            ) }
            fn visit_pat_macro          (&mut self, _: &'_ PatMacro         ) { _(_, _, visit::visit_pat_macro          ) }
            fn visit_pat_or             (&mut self, _: &'_ PatOr            ) { _(_, _, visit::visit_pat_or             ) }
            fn visit_pat_path           (&mut self, _: &'_ PatPath          ) { _(_, _, visit::visit_pat_path           ) }
            fn visit_pat_range          (&mut self, _: &'_ PatRange         ) { _(_, _, visit::visit_pat_range          ) }
            fn visit_pat_reference      (&mut self, _: &'_ PatReference     ) { _(_, _, visit::visit_pat_reference      ) }
            fn visit_pat_rest           (&mut self, _: &'_ PatRest          ) { _(_, _, visit::visit_pat_rest           ) }
            fn visit_pat_slice          (&mut self, _: &'_ PatSlice         ) { _(_, _, visit::visit_pat_slice          ) }
            fn visit_pat_struct         (&mut self, _: &'_ PatStruct        ) { _(_, _, visit::visit_pat_struct         ) }
            fn visit_pat_tuple          (&mut self, _: &'_ PatTuple         ) { _(_, _, visit::visit_pat_tuple          ) }
            fn visit_pat_tuple_struct   (&mut self, _: &'_ PatTupleStruct   ) { _(_, _, visit::visit_pat_tuple_struct   ) }
            fn visit_pat_type           (&mut self, _: &'_ PatType          ) { _(_, _, visit::visit_pat_type           ) }
            fn visit_pat_wild           (&mut self, _: &'_ PatWild          ) { _(_, _, visit::visit_pat_wild           ) }
            fn visit_receiver           (&mut self, _: &'_ Receiver         ) { _(_, _, visit::visit_receiver           ) }
            fn visit_trait_item_const   (&mut self, _: &'_ TraitItemConst   ) { _(_, _, visit::visit_trait_item_const   ) }
            fn visit_trait_item_macro   (&mut self, _: &'_ TraitItemMacro   ) { _(_, _, visit::visit_trait_item_macro   ) }
            fn visit_trait_item_method  (&mut self, _: &'_ TraitItemMethod  ) { _(_, _, visit::visit_trait_item_method  ) }
            fn visit_trait_item_type    (&mut self, _: &'_ TraitItemType    ) { _(_, _, visit::visit_trait_item_type    ) }
            fn visit_type_param         (&mut self, _: &'_ TypeParam        ) { _(_, _, visit::visit_type_param         ) }
            fn visit_variadic           (&mut self, _: &'_ Variadic         ) { _(_, _, visit::visit_variadic           ) }
            fn visit_variant            (&mut self, _: &'_ Variant          ) { _(_, _, visit::visit_variant            ) }
        }
    }

    let file = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?;

    let mut replacements = btreemap!();

    Visitor {
        replacements: &mut replacements,
        features,
    }
    .visit_file(&file);

    Ok(replace_ranges(code, replacements))
}

pub(crate) fn erase_docs(code: &str) -> anyhow::Result<String> {
    struct Visitor<'a>(&'a mut [FixedBitSet]);

    impl Visit<'_> for Visitor<'_> {
        fn visit_attribute(&mut self, attr: &'_ Attribute) {
            if matches!(
                attr.parse_meta(), Ok(m) if matches!(m.path().get_ident(), Some(i) if i == "doc")
            ) {
                set_span(self.0, attr.span(), true);
            }
        }
    }

    erase(code, |mask, token_stream| {
        syn::parse2(token_stream).map(|f| Visitor(mask).visit_file(&f))
    })
}

pub(crate) fn erase_comments(code: &str) -> anyhow::Result<String> {
    fn visit_file(mask: &mut [FixedBitSet], token_stream: TokenStream) -> syn::Result<()> {
        fn visit_token_stream(mask: &mut [FixedBitSet], token_stream: TokenStream) {
            for tt in token_stream {
                if let TokenTree::Group(group) = tt {
                    set_span(mask, group.span_open(), false);
                    visit_token_stream(mask, group.stream());
                    set_span(mask, group.span_close(), false);
                } else {
                    set_span(mask, tt.span(), false);
                }
            }
        }

        for mask in &mut *mask {
            mask.insert_range(..);
        }
        visit_token_stream(mask, token_stream);
        Ok(())
    }

    erase(code, visit_file)
}

fn erase(
    code: &str,
    visit_file: fn(&mut [FixedBitSet], TokenStream) -> syn::Result<()>,
) -> anyhow::Result<String> {
    let code = if code.starts_with("#!") {
        let (_, code) = code.split_at(code.find('\n').unwrap_or_else(|| code.len()));
        code
    } else {
        code
    };

    let token_stream = code
        .parse::<TokenStream>()
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could lex the code")?;

    let mut erase = code
        .lines()
        .map(|l| FixedBitSet::with_capacity(l.chars().count()))
        .collect::<Vec<_>>();

    visit_file(&mut erase, token_stream)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could parse the code")?;

    let mut acc = "".to_owned();
    for (line, erase) in code.lines().zip_eq(erase) {
        for (j, ch) in line.chars().enumerate() {
            acc.push(if erase[j] { ' ' } else { ch });
        }
        acc += "\n";
    }
    Ok(acc.trim_start().to_owned())
}

fn set_span(mask: &mut [FixedBitSet], span: Span, p: bool) {
    if span.start().line == span.end().line {
        let i = span.start().line - 1;
        let l = span.start().column;
        let r = span.end().column;
        mask[i].set_range(l..r, p);
    } else {
        let i1 = span.start().line - 1;
        let i2 = span.end().line - 1;
        let l = span.start().column;
        mask[i1].insert_range(l..);
        for mask in &mut mask[i1 + 1..i2] {
            mask.set_range(.., p);
        }
        let r = span.end().column;
        mask[i2].set_range(..r, p);
    }
}

pub(crate) fn minify(code: &str, shell: &mut Shell, name: Option<&str>) -> anyhow::Result<String> {
    fn minify(acc: &mut String, token_stream: TokenStream) {
        #[derive(PartialEq)]
        enum Prev {
            None,
            IdentOrLit,
            Puncts(String, Spacing),
        }

        let mut prev = Prev::None;
        for tt in token_stream {
            match tt {
                TokenTree::Group(group) => {
                    if let Prev::Puncts(puncts, _) = mem::replace(&mut prev, Prev::None) {
                        *acc += &puncts;
                    }
                    let (left, right) = match group.delimiter() {
                        proc_macro2::Delimiter::Parenthesis => ('(', ')'),
                        proc_macro2::Delimiter::Brace => ('{', '}'),
                        proc_macro2::Delimiter::Bracket => ('[', ']'),
                        proc_macro2::Delimiter::None => (' ', ' '),
                    };
                    acc.push(left);
                    minify(acc, group.stream());
                    acc.push(right);
                    prev = Prev::None;
                }
                TokenTree::Ident(ident) => {
                    match mem::replace(&mut prev, Prev::IdentOrLit) {
                        Prev::IdentOrLit => *acc += " ",
                        Prev::Puncts(puncts, _) => *acc += &puncts,
                        _ => {}
                    }
                    *acc += &ident.to_string();
                }
                TokenTree::Literal(literal) => {
                    match mem::replace(&mut prev, Prev::IdentOrLit) {
                        Prev::IdentOrLit => *acc += " ",
                        Prev::Puncts(puncts, _) => *acc += &puncts,
                        _ => {}
                    }
                    *acc += &literal.to_string();
                }
                TokenTree::Punct(punct) => {
                    if let Prev::Puncts(puncts, spacing) = &mut prev {
                        if *spacing == Spacing::Alone {
                            *acc += puncts;
                            // https://docs.rs/syn/1.0.46/syn/token/index.html
                            if [
                                ("!", '='),
                                ("%", '='),
                                ("&", '&'),
                                ("&", '='),
                                ("*", '='),
                                ("+", '='),
                                ("-", '='),
                                ("-", '>'),
                                (".", '.'),
                                ("..", '.'),
                                ("..", '='),
                                ("/", '='),
                                (":", ':'),
                                ("<", '-'),
                                ("<", '<'),
                                ("<", '='),
                                ("<<", '='),
                                ("=", '='),
                                ("=", '>'),
                                (">", '='),
                                (">", '>'),
                                (">>", '='),
                                ("^", '='),
                                ("|", '='),
                                ("|", '|'),
                            ]
                            .contains(&(&&*puncts, punct.as_char()))
                            {
                                *acc += " ";
                            }
                            prev = Prev::Puncts(punct.as_char().to_string(), punct.spacing());
                        } else {
                            puncts.push(punct.as_char());
                            *spacing = punct.spacing();
                        }
                    } else {
                        prev = Prev::Puncts(punct.as_char().to_string(), punct.spacing());
                    }
                }
            }
        }
        if let Prev::Puncts(puncts, _) = prev {
            *acc += &puncts;
        }
    }

    let token_stream = syn::parse_file(code)
        .map_err(|e| anyhow!("{:?}", e))
        .with_context(|| "could not parse the code")?
        .into_token_stream();

    let safe = token_stream.to_string();

    let mut acc = "".to_owned();
    minify(&mut acc, token_stream);

    if syn::parse_file(&acc).is_ok() {
        Ok(acc)
    } else {
        shell.warn(format!(
            "could not minify the code. inserting spaces{}",
            name.map(|s| format!(": `{}`", s)).unwrap_or_default(),
        ))?;
        Ok(safe)
    }
}

#[cfg(test)]
mod tests {
    use difference::assert_diff;

    #[test]
    fn modify_macros() -> anyhow::Result<()> {
        fn test(input: &str, expected: &str) -> anyhow::Result<()> {
            static EXTERN_CRATE_NAME: &str = "lib";
            let actual = super::modify_macros(input, EXTERN_CRATE_NAME)?;
            assert_diff!(expected, &actual, "\n", 0);
            Ok(())
        }

        test(
            r#"#[cfg_attr(cargo_equip, cargo_equip::translate_dollar_crates)]
#[macro_export]
macro_rules! hello {
    (1 $(,)?) => {
        $crate::hello::hello();
        $crate::__hello_inner!($n)
    };
    (0 $(,)?) => {};
}

macro_rules! _without_attr {
    () => {
        let _ = $crate::hello;
        $crate::hello!(0);
    };
}
"#,
            r#"#[cfg_attr(cargo_equip, cargo_equip::translate_dollar_crates)]
#[macro_export]
macro_rules! hello {
    (1 $(,)?) => {
        $crate::lib::hello::hello();
        $crate::__hello_inner!($n)
    };
    (0 $(,)?) => {};
}

macro_rules! _without_attr {
    () => {
        let _ = $crate::hello;
        $crate::hello!(0);
    };
}
"#,
        )
    }

    #[test]
    fn prepend_mod_doc() -> syn::Result<()> {
        fn test(code: &str, append: &str, expected: &str) -> syn::Result<()> {
            let actual = super::prepend_mod_doc(code, append)?;
            assert_diff!(expected, &actual, "\n", 0);
            Ok(())
        }

        test(
            r#"//! aaaaaaa
//! bbbbbbb
//! ccccccc

fn main() {
    todo!();
}
"#,
            r" ddddddd
",
            r#"//! aaaaaaa
//! bbbbbbb
//! ccccccc
//!
//! ddddddd

fn main() {
    todo!();
}
"#,
        )?;
        test(
            r#"fn main() {
    todo!();
}
"#,
            r" dddddd
",
            r#"//! dddddd

fn main() {
    todo!();
}
"#,
        )?;
        Ok(())
    }

    #[test]
    fn erase_docs() -> anyhow::Result<()> {
        fn test(input: &str, expected: &str) -> anyhow::Result<()> {
            let actual = super::erase_docs(input)?;
            assert_diff!(expected, &actual, "\n", 0);
            Ok(())
        }

        test(
            r#"//! aaaaa
//! bbbbb

fn main() {}

/// ccccc
struct Foo;
"#,
            r#"fn main() {}

         
struct Foo;
"#,
        )?;

        test(
            r#"//! モジュールのドキュメント

/// アイテムのドキュメント
fn foo() {}
"#,
            r#"fn foo() {}
"#,
        )
    }

    #[test]
    fn erase_comments() -> anyhow::Result<()> {
        fn test(input: &str, expected: &str) -> anyhow::Result<()> {
            let actual = super::erase_comments(input)?;
            assert_diff!(expected, &actual, "\n", 0);
            Ok(())
        }

        test(
            r#"// aaaaa
// bbbbb
fn main() {
    // ccccc
    /*ddddd*/println!("Hi!");/*eeeee*/
    // fffff
}
// ggggg
"#,
            r#"fn main() {
            
             println!("Hi!");         
            
}
        
"#,
        )?;

        test(
            r#"/* aaaaa */ type A = (i64, i64, i64); // bbbbb
"#,
            r#"type A = (i64, i64, i64);         
"#,
        )?;

        test(
            r#"// あああ
/*いいい*/fn foo() {
    let _ = 1 + 1; // ううううう
}
"#,
            r#"fn foo() {
    let _ = 1 + 1;         
}
"#,
        )
    }
}
