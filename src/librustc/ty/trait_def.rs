// Copyright 2012-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use hir;
use hir::def_id::DefId;
use hir::map::DefPathHash;
use ich::{self, StableHashingContext};
use traits::specialization_graph;
use ty::fast_reject;
use ty::fold::TypeFoldable;
use ty::{Ty, TyCtxt};

use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::stable_hasher::{HashStable, StableHasher,
                                           StableHasherResult};
use rustc_data_structures::sync::Lrc;

/// A trait's definition with type information.
pub struct TraitDef {
    pub def_id: DefId,

    pub unsafety: hir::Unsafety,

    /// If `true`, then this trait had the `#[rustc_paren_sugar]`
    /// attribute, indicating that it should be used with `Foo()`
    /// sugar. This is a temporary thing -- eventually any trait will
    /// be usable with the sugar (or without it).
    pub paren_sugar: bool,

    pub has_auto_impl: bool,

    /// The ICH of this trait's DefPath, cached here so it doesn't have to be
    /// recomputed all the time.
    pub def_path_hash: DefPathHash,
}

#[derive(Default)]
pub struct TraitImpls {
    blanket_impls: Vec<DefId>,
    /// Impls indexed by their simplified self-type, for fast lookup.
    non_blanket_impls: FxHashMap<fast_reject::SimplifiedType, Vec<DefId>>,
}

impl<'a, 'gcx, 'tcx> TraitDef {
    pub fn new(def_id: DefId,
               unsafety: hir::Unsafety,
               paren_sugar: bool,
               has_auto_impl: bool,
               def_path_hash: DefPathHash)
               -> TraitDef {
        TraitDef {
            def_id,
            paren_sugar,
            unsafety,
            has_auto_impl,
            def_path_hash,
        }
    }

    pub fn ancestors(&self, tcx: TyCtxt<'a, 'gcx, 'tcx>,
                     of_impl: DefId)
                     -> specialization_graph::Ancestors {
        specialization_graph::ancestors(tcx, self.def_id, of_impl)
    }
}

impl<'a, 'gcx, 'tcx> TyCtxt<'a, 'gcx, 'tcx> {
    pub fn for_each_impl<F: FnMut(DefId)>(self, def_id: DefId, mut f: F) {
        let impls = self.trait_impls_of(def_id);

        for &impl_def_id in impls.blanket_impls.iter() {
            f(impl_def_id);
        }

        for v in impls.non_blanket_impls.values() {
            for &impl_def_id in v {
                f(impl_def_id);
            }
        }
    }

    /// Iterate over every impl that could possibly match the
    /// self-type `self_ty`.
    pub fn for_each_relevant_impl<F: FnMut(DefId)>(self,
                                                   def_id: DefId,
                                                   self_ty: Ty<'tcx>,
                                                   mut f: F)
    {
        let impls = self.trait_impls_of(def_id);

        for &impl_def_id in impls.blanket_impls.iter() {
            f(impl_def_id);
        }

        // simplify_type(.., false) basically replaces type parameters and
        // projections with infer-variables. This is, of course, done on
        // the impl trait-ref when it is instantiated, but not on the
        // predicate trait-ref which is passed here.
        //
        // for example, if we match `S: Copy` against an impl like
        // `impl<T:Copy> Copy for Option<T>`, we replace the type variable
        // in `Option<T>` with an infer variable, to `Option<_>` (this
        // doesn't actually change fast_reject output), but we don't
        // replace `S` with anything - this impl of course can't be
        // selected, and as there are hundreds of similar impls,
        // considering them would significantly harm performance.

        // This depends on the set of all impls for the trait. That is
        // unfortunate. When we get red-green recompilation, we would like
        // to have a way of knowing whether the set of relevant impls
        // changed. The most naive
        // way would be to compute the Vec of relevant impls and see whether
        // it differs between compilations. That shouldn't be too slow by
        // itself - we do quite a bit of work for each relevant impl anyway.
        //
        // If we want to be faster, we could have separate queries for
        // blanket and non-blanket impls, and compare them separately.
        //
        // I think we'll cross that bridge when we get to it.
        if let Some(simp) = fast_reject::simplify_type(self, self_ty, true) {
            if let Some(impls) = impls.non_blanket_impls.get(&simp) {
                for &impl_def_id in impls {
                    f(impl_def_id);
                }
            }
        } else {
            for v in impls.non_blanket_impls.values() {
                for &impl_def_id in v {
                    f(impl_def_id);
                }
            }
        }
    }

    /// Return a vector containing all impls
    pub fn all_impls(self, def_id: DefId) -> Vec<DefId> {
        let impls = self.trait_impls_of(def_id);

        impls.blanket_impls.iter().chain(
            impls.non_blanket_impls.values().flatten()
        ).cloned().collect()
    }
}

// Query provider for `trait_impls_of`.
pub(super) fn trait_impls_of_provider<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                                trait_id: DefId)
                                                -> Lrc<TraitImpls> {
    let mut impls = TraitImpls::default();

    {
        let mut add_impl = |impl_def_id| {
            let impl_self_ty = tcx.type_of(impl_def_id);
            if impl_def_id.is_local() && impl_self_ty.references_error() {
                return;
            }

            if let Some(simplified_self_ty) =
                fast_reject::simplify_type(tcx, impl_self_ty, false)
            {
                impls.non_blanket_impls
                    .entry(simplified_self_ty)
                    .or_default()
                    .push(impl_def_id);
            } else {
                impls.blanket_impls.push(impl_def_id);
            }
        };

        // Traits defined in the current crate can't have impls in upstream
        // crates, so we don't bother querying the cstore.
        if !trait_id.is_local() {
            for &cnum in tcx.crates().iter() {
                for &def_id in tcx.implementations_of_trait((cnum, trait_id)).iter() {
                    add_impl(def_id);
                }
            }
        }

        for &node_id in tcx.hir.trait_impls(trait_id) {
            add_impl(tcx.hir.local_def_id(node_id));
        }
    }

    Lrc::new(impls)
}

impl<'a> HashStable<StableHashingContext<'a>> for TraitImpls {
    fn hash_stable<W: StableHasherResult>(&self,
                                          hcx: &mut StableHashingContext<'a>,
                                          hasher: &mut StableHasher<W>) {
        let TraitImpls {
            ref blanket_impls,
            ref non_blanket_impls,
        } = *self;

        ich::hash_stable_trait_impls(hcx, hasher, blanket_impls, non_blanket_impls);
    }
}
