// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Helper routines for higher-ranked things. See the `doc` module at
//! the end of the file for details.

use super::{CombinedSnapshot, InferCtxt, HigherRankedType, SkolemizationMap};
use super::combine::CombineFields;

use ty::{self, TyCtxt, Binder, TypeFoldable};
use ty::error::TypeError;
use ty::relate::{Relate, RelateResult, TypeRelation};
use syntax::codemap::Span;
use util::nodemap::{FnvHashMap, FnvHashSet};

pub trait HigherRankedRelations<'a,'tcx> {
    fn higher_ranked_sub<T>(&self, a: &Binder<T>, b: &Binder<T>) -> RelateResult<'tcx, Binder<T>>
        where T: Relate<'a,'tcx>;

    fn higher_ranked_lub<T>(&self, a: &Binder<T>, b: &Binder<T>) -> RelateResult<'tcx, Binder<T>>
        where T: Relate<'a,'tcx>;

    fn higher_ranked_glb<T>(&self, a: &Binder<T>, b: &Binder<T>) -> RelateResult<'tcx, Binder<T>>
        where T: Relate<'a,'tcx>;
}

trait InferCtxtExt {
    fn tainted_regions(&self, snapshot: &CombinedSnapshot, r: ty::Region) -> Vec<ty::Region>;

    fn region_vars_confined_to_snapshot(&self,
                                        snapshot: &CombinedSnapshot)
                                        -> Vec<ty::RegionVid>;
}

impl<'a,'tcx> HigherRankedRelations<'a,'tcx> for CombineFields<'a,'tcx> {
    fn higher_ranked_sub<T>(&self, a: &Binder<T>, b: &Binder<T>)
                            -> RelateResult<'tcx, Binder<T>>
        where T: Relate<'a,'tcx>
    {
        debug!("higher_ranked_sub(a={:?}, b={:?})",
               a, b);

        // Rather than checking the subtype relationship between `a` and `b`
        // as-is, we need to do some extra work here in order to make sure
        // that function subtyping works correctly with respect to regions
        //
        // Note: this is a subtle algorithm.  For a full explanation,
        // please see the large comment at the end of the file in the (inlined) module
        // `doc`.

        // Start a snapshot so we can examine "all bindings that were
        // created as part of this type comparison".
        return self.infcx.commit_if_ok(|snapshot| {
            // First, we instantiate each bound region in the subtype with a fresh
            // region variable.
            let (a_prime, _) =
                self.infcx.replace_late_bound_regions_with_fresh_var(
                    self.trace.origin.span(),
                    HigherRankedType,
                    a);

            // Second, we instantiate each bound region in the supertype with a
            // fresh concrete region.
            let (b_prime, skol_map) =
                self.infcx.skolemize_late_bound_regions(b, snapshot);

            debug!("a_prime={:?}", a_prime);
            debug!("b_prime={:?}", b_prime);

            // Compare types now that bound regions have been replaced.
            let result = self.sub().relate(&a_prime, &b_prime)?;

            // Presuming type comparison succeeds, we need to check
            // that the skolemized regions do not "leak".
            match leak_check(self.infcx, &skol_map, snapshot) {
                Ok(()) => { }
                Err((skol_br, tainted_region)) => {
                    if self.a_is_expected {
                        debug!("Not as polymorphic!");
                        return Err(TypeError::RegionsInsufficientlyPolymorphic(skol_br,
                                                                               tainted_region));
                    } else {
                        debug!("Overly polymorphic!");
                        return Err(TypeError::RegionsOverlyPolymorphic(skol_br,
                                                                       tainted_region));
                    }
                }
            }

            debug!("higher_ranked_sub: OK result={:?}",
                   result);

            Ok(ty::Binder(result))
        });
    }

    fn higher_ranked_lub<T>(&self, a: &Binder<T>, b: &Binder<T>) -> RelateResult<'tcx, Binder<T>>
        where T: Relate<'a,'tcx>
    {
        // Start a snapshot so we can examine "all bindings that were
        // created as part of this type comparison".
        return self.infcx.commit_if_ok(|snapshot| {
            // Instantiate each bound region with a fresh region variable.
            let span = self.trace.origin.span();
            let (a_with_fresh, a_map) =
                self.infcx.replace_late_bound_regions_with_fresh_var(
                    span, HigherRankedType, a);
            let (b_with_fresh, _) =
                self.infcx.replace_late_bound_regions_with_fresh_var(
                    span, HigherRankedType, b);

            // Collect constraints.
            let result0 =
                self.lub().relate(&a_with_fresh, &b_with_fresh)?;
            let result0 =
                self.infcx.resolve_type_vars_if_possible(&result0);
            debug!("lub result0 = {:?}", result0);

            // Generalize the regions appearing in result0 if possible
            let new_vars = self.infcx.region_vars_confined_to_snapshot(snapshot);
            let span = self.trace.origin.span();
            let result1 =
                fold_regions_in(
                    self.tcx(),
                    &result0,
                    |r, debruijn| generalize_region(self.infcx, span, snapshot, debruijn,
                                                    &new_vars, &a_map, r));

            debug!("lub({:?},{:?}) = {:?}",
                   a,
                   b,
                   result1);

            Ok(ty::Binder(result1))
        });

        fn generalize_region(infcx: &InferCtxt,
                             span: Span,
                             snapshot: &CombinedSnapshot,
                             debruijn: ty::DebruijnIndex,
                             new_vars: &[ty::RegionVid],
                             a_map: &FnvHashMap<ty::BoundRegion, ty::Region>,
                             r0: ty::Region)
                             -> ty::Region {
            // Regions that pre-dated the LUB computation stay as they are.
            if !is_var_in_set(new_vars, r0) {
                assert!(!r0.is_bound());
                debug!("generalize_region(r0={:?}): not new variable", r0);
                return r0;
            }

            let tainted = infcx.tainted_regions(snapshot, r0);

            // Variables created during LUB computation which are
            // *related* to regions that pre-date the LUB computation
            // stay as they are.
            if !tainted.iter().all(|r| is_var_in_set(new_vars, *r)) {
                debug!("generalize_region(r0={:?}): \
                        non-new-variables found in {:?}",
                       r0, tainted);
                assert!(!r0.is_bound());
                return r0;
            }

            // Otherwise, the variable must be associated with at
            // least one of the variables representing bound regions
            // in both A and B.  Replace the variable with the "first"
            // bound region from A that we find it to be associated
            // with.
            for (a_br, a_r) in a_map {
                if tainted.iter().any(|x| x == a_r) {
                    debug!("generalize_region(r0={:?}): \
                            replacing with {:?}, tainted={:?}",
                           r0, *a_br, tainted);
                    return ty::ReLateBound(debruijn, *a_br);
                }
            }

            span_bug!(
                span,
                "region {:?} is not associated with any bound region from A!",
                r0)
        }
    }

    fn higher_ranked_glb<T>(&self, a: &Binder<T>, b: &Binder<T>) -> RelateResult<'tcx, Binder<T>>
        where T: Relate<'a,'tcx>
    {
        debug!("higher_ranked_glb({:?}, {:?})",
               a, b);

        // Make a snapshot so we can examine "all bindings that were
        // created as part of this type comparison".
        return self.infcx.commit_if_ok(|snapshot| {
            // Instantiate each bound region with a fresh region variable.
            let (a_with_fresh, a_map) =
                self.infcx.replace_late_bound_regions_with_fresh_var(
                    self.trace.origin.span(), HigherRankedType, a);
            let (b_with_fresh, b_map) =
                self.infcx.replace_late_bound_regions_with_fresh_var(
                    self.trace.origin.span(), HigherRankedType, b);
            let a_vars = var_ids(self, &a_map);
            let b_vars = var_ids(self, &b_map);

            // Collect constraints.
            let result0 =
                self.glb().relate(&a_with_fresh, &b_with_fresh)?;
            let result0 =
                self.infcx.resolve_type_vars_if_possible(&result0);
            debug!("glb result0 = {:?}", result0);

            // Generalize the regions appearing in result0 if possible
            let new_vars = self.infcx.region_vars_confined_to_snapshot(snapshot);
            let span = self.trace.origin.span();
            let result1 =
                fold_regions_in(
                    self.tcx(),
                    &result0,
                    |r, debruijn| generalize_region(self.infcx, span, snapshot, debruijn,
                                                    &new_vars,
                                                    &a_map, &a_vars, &b_vars,
                                                    r));

            debug!("glb({:?},{:?}) = {:?}",
                   a,
                   b,
                   result1);

            Ok(ty::Binder(result1))
        });

        fn generalize_region(infcx: &InferCtxt,
                             span: Span,
                             snapshot: &CombinedSnapshot,
                             debruijn: ty::DebruijnIndex,
                             new_vars: &[ty::RegionVid],
                             a_map: &FnvHashMap<ty::BoundRegion, ty::Region>,
                             a_vars: &[ty::RegionVid],
                             b_vars: &[ty::RegionVid],
                             r0: ty::Region) -> ty::Region {
            if !is_var_in_set(new_vars, r0) {
                assert!(!r0.is_bound());
                return r0;
            }

            let tainted = infcx.tainted_regions(snapshot, r0);

            let mut a_r = None;
            let mut b_r = None;
            let mut only_new_vars = true;
            for r in &tainted {
                if is_var_in_set(a_vars, *r) {
                    if a_r.is_some() {
                        return fresh_bound_variable(infcx, debruijn);
                    } else {
                        a_r = Some(*r);
                    }
                } else if is_var_in_set(b_vars, *r) {
                    if b_r.is_some() {
                        return fresh_bound_variable(infcx, debruijn);
                    } else {
                        b_r = Some(*r);
                    }
                } else if !is_var_in_set(new_vars, *r) {
                    only_new_vars = false;
                }
            }

            // NB---I do not believe this algorithm computes
            // (necessarily) the GLB.  As written it can
            // spuriously fail. In particular, if there is a case
            // like: |fn(&a)| and fn(fn(&b)), where a and b are
            // free, it will return fn(&c) where c = GLB(a,b).  If
            // however this GLB is not defined, then the result is
            // an error, even though something like
            // "fn<X>(fn(&X))" where X is bound would be a
            // subtype of both of those.
            //
            // The problem is that if we were to return a bound
            // variable, we'd be computing a lower-bound, but not
            // necessarily the *greatest* lower-bound.
            //
            // Unfortunately, this problem is non-trivial to solve,
            // because we do not know at the time of computing the GLB
            // whether a GLB(a,b) exists or not, because we haven't
            // run region inference (or indeed, even fully computed
            // the region hierarchy!). The current algorithm seems to
            // works ok in practice.

            if a_r.is_some() && b_r.is_some() && only_new_vars {
                // Related to exactly one bound variable from each fn:
                return rev_lookup(span, a_map, a_r.unwrap());
            } else if a_r.is_none() && b_r.is_none() {
                // Not related to bound variables from either fn:
                assert!(!r0.is_bound());
                return r0;
            } else {
                // Other:
                return fresh_bound_variable(infcx, debruijn);
            }
        }

        fn rev_lookup(span: Span,
                      a_map: &FnvHashMap<ty::BoundRegion, ty::Region>,
                      r: ty::Region) -> ty::Region
        {
            for (a_br, a_r) in a_map {
                if *a_r == r {
                    return ty::ReLateBound(ty::DebruijnIndex::new(1), *a_br);
                }
            }
            span_bug!(
                span,
                "could not find original bound region for {:?}",
                r);
        }

        fn fresh_bound_variable(infcx: &InferCtxt, debruijn: ty::DebruijnIndex) -> ty::Region {
            infcx.region_vars.new_bound(debruijn)
        }
    }
}

fn var_ids<'a, 'tcx>(fields: &CombineFields<'a, 'tcx>,
                      map: &FnvHashMap<ty::BoundRegion, ty::Region>)
                     -> Vec<ty::RegionVid> {
    map.iter()
       .map(|(_, r)| match *r {
           ty::ReVar(r) => { r }
           r => {
               span_bug!(
                   fields.trace.origin.span(),
                   "found non-region-vid: {:?}",
                   r);
           }
       })
       .collect()
}

fn is_var_in_set(new_vars: &[ty::RegionVid], r: ty::Region) -> bool {
    match r {
        ty::ReVar(ref v) => new_vars.iter().any(|x| x == v),
        _ => false
    }
}

fn fold_regions_in<'tcx, T, F>(tcx: &TyCtxt<'tcx>,
                               unbound_value: &T,
                               mut fldr: F)
                               -> T
    where T: TypeFoldable<'tcx>,
          F: FnMut(ty::Region, ty::DebruijnIndex) -> ty::Region,
{
    tcx.fold_regions(unbound_value, &mut false, |region, current_depth| {
        // we should only be encountering "escaping" late-bound regions here,
        // because the ones at the current level should have been replaced
        // with fresh variables
        assert!(match region {
            ty::ReLateBound(..) => false,
            _ => true
        });

        fldr(region, ty::DebruijnIndex::new(current_depth))
    })
}

impl<'a,'tcx> InferCtxtExt for InferCtxt<'a,'tcx> {
    fn tainted_regions(&self, snapshot: &CombinedSnapshot, r: ty::Region) -> Vec<ty::Region> {
        self.region_vars.tainted(&snapshot.region_vars_snapshot, r)
    }

    fn region_vars_confined_to_snapshot(&self,
                                        snapshot: &CombinedSnapshot)
                                        -> Vec<ty::RegionVid>
    {
        /*!
         * Returns the set of region variables that do not affect any
         * types/regions which existed before `snapshot` was
         * started. This is used in the sub/lub/glb computations. The
         * idea here is that when we are computing lub/glb of two
         * regions, we sometimes create intermediate region variables.
         * Those region variables may touch some of the skolemized or
         * other "forbidden" regions we created to replace bound
         * regions, but they don't really represent an "external"
         * constraint.
         *
         * However, sometimes fresh variables are created for other
         * purposes too, and those *may* represent an external
         * constraint. In particular, when a type variable is
         * instantiated, we create region variables for all the
         * regions that appear within, and if that type variable
         * pre-existed the snapshot, then those region variables
         * represent external constraints.
         *
         * An example appears in the unit test
         * `sub_free_bound_false_infer`.  In this test, we want to
         * know whether
         *
         * ```rust
         * fn(_#0t) <: for<'a> fn(&'a int)
         * ```
         *
         * Note that the subtype has a type variable. Because the type
         * variable can't be instantiated with a region that is bound
         * in the fn signature, this comparison ought to fail. But if
         * we're not careful, it will succeed.
         *
         * The reason is that when we walk through the subtyping
         * algorith, we begin by replacing `'a` with a skolemized
         * variable `'1`. We then have `fn(_#0t) <: fn(&'1 int)`. This
         * can be made true by unifying `_#0t` with `&'1 int`. In the
         * process, we create a fresh variable for the skolemized
         * region, `'$2`, and hence we have that `_#0t == &'$2
         * int`. However, because `'$2` was created during the sub
         * computation, if we're not careful we will erroneously
         * assume it is one of the transient region variables
         * representing a lub/glb internally. Not good.
         *
         * To prevent this, we check for type variables which were
         * unified during the snapshot, and say that any region
         * variable created during the snapshot but which finds its
         * way into a type variable is considered to "escape" the
         * snapshot.
         */

        let mut region_vars =
            self.region_vars.vars_created_since_snapshot(&snapshot.region_vars_snapshot);

        let escaping_types =
            self.type_variables.borrow_mut().types_escaping_snapshot(&snapshot.type_snapshot);

        let mut escaping_region_vars = FnvHashSet();
        for ty in &escaping_types {
            self.tcx.collect_regions(ty, &mut escaping_region_vars);
        }

        region_vars.retain(|&region_vid| {
            let r = ty::ReVar(region_vid);
            !escaping_region_vars.contains(&r)
        });

        debug!("region_vars_confined_to_snapshot: region_vars={:?} escaping_types={:?}",
               region_vars,
               escaping_types);

        region_vars
    }
}

pub fn skolemize_late_bound_regions<'a,'tcx,T>(infcx: &InferCtxt<'a,'tcx>,
                                               binder: &ty::Binder<T>,
                                               snapshot: &CombinedSnapshot)
                                               -> (T, SkolemizationMap)
    where T : TypeFoldable<'tcx>
{
    /*!
     * Replace all regions bound by `binder` with skolemized regions and
     * return a map indicating which bound-region was replaced with what
     * skolemized region. This is the first step of checking subtyping
     * when higher-ranked things are involved. See `README.md` for more
     * details.
     */

    let (result, map) = infcx.tcx.replace_late_bound_regions(binder, |br| {
        infcx.region_vars.new_skolemized(br, &snapshot.region_vars_snapshot)
    });

    debug!("skolemize_bound_regions(binder={:?}, result={:?}, map={:?})",
           binder,
           result,
           map);

    (result, map)
}

pub fn leak_check<'a,'tcx>(infcx: &InferCtxt<'a,'tcx>,
                           skol_map: &SkolemizationMap,
                           snapshot: &CombinedSnapshot)
                           -> Result<(),(ty::BoundRegion,ty::Region)>
{
    /*!
     * Searches the region constriants created since `snapshot` was started
     * and checks to determine whether any of the skolemized regions created
     * in `skol_map` would "escape" -- meaning that they are related to
     * other regions in some way. If so, the higher-ranked subtyping doesn't
     * hold. See `README.md` for more details.
     */

    debug!("leak_check: skol_map={:?}",
           skol_map);

    let new_vars = infcx.region_vars_confined_to_snapshot(snapshot);
    for (&skol_br, &skol) in skol_map {
        let tainted = infcx.tainted_regions(snapshot, skol);
        for &tainted_region in &tainted {
            // Each skolemized should only be relatable to itself
            // or new variables:
            match tainted_region {
                ty::ReVar(vid) => {
                    if new_vars.iter().any(|&x| x == vid) { continue; }
                }
                _ => {
                    if tainted_region == skol { continue; }
                }
            };

            debug!("{:?} (which replaced {:?}) is tainted by {:?}",
                   skol,
                   skol_br,
                   tainted_region);

            // A is not as polymorphic as B:
            return Err((skol_br, tainted_region));
        }
    }
    Ok(())
}

/// This code converts from skolemized regions back to late-bound
/// regions. It works by replacing each region in the taint set of a
/// skolemized region with a bound-region. The bound region will be bound
/// by the outer-most binder in `value`; the caller must ensure that there is
/// such a binder and it is the right place.
///
/// This routine is only intended to be used when the leak-check has
/// passed; currently, it's used in the trait matching code to create
/// a set of nested obligations frmo an impl that matches against
/// something higher-ranked.  More details can be found in
/// `librustc/middle/traits/README.md`.
///
/// As a brief example, consider the obligation `for<'a> Fn(&'a int)
/// -> &'a int`, and the impl:
///
///     impl<A,R> Fn<A,R> for SomethingOrOther
///         where A : Clone
///     { ... }
///
/// Here we will have replaced `'a` with a skolemized region
/// `'0`. This means that our substitution will be `{A=>&'0
/// int, R=>&'0 int}`.
///
/// When we apply the substitution to the bounds, we will wind up with
/// `&'0 int : Clone` as a predicate. As a last step, we then go and
/// replace `'0` with a late-bound region `'a`.  The depth is matched
/// to the depth of the predicate, in this case 1, so that the final
/// predicate is `for<'a> &'a int : Clone`.
pub fn plug_leaks<'a,'tcx,T>(infcx: &InferCtxt<'a,'tcx>,
                             skol_map: SkolemizationMap,
                             snapshot: &CombinedSnapshot,
                             value: &T)
                             -> T
    where T : TypeFoldable<'tcx>
{
    debug_assert!(leak_check(infcx, &skol_map, snapshot).is_ok());

    debug!("plug_leaks(skol_map={:?}, value={:?})",
           skol_map,
           value);

    // Compute a mapping from the "taint set" of each skolemized
    // region back to the `ty::BoundRegion` that it originally
    // represented. Because `leak_check` passed, we know that
    // these taint sets are mutually disjoint.
    let inv_skol_map: FnvHashMap<ty::Region, ty::BoundRegion> =
        skol_map
        .into_iter()
        .flat_map(|(skol_br, skol)| {
            infcx.tainted_regions(snapshot, skol)
                .into_iter()
                .map(move |tainted_region| (tainted_region, skol_br))
        })
        .collect();

    debug!("plug_leaks: inv_skol_map={:?}",
           inv_skol_map);

    // Remove any instantiated type variables from `value`; those can hide
    // references to regions from the `fold_regions` code below.
    let value = infcx.resolve_type_vars_if_possible(value);

    // Map any skolemization byproducts back to a late-bound
    // region. Put that late-bound region at whatever the outermost
    // binder is that we encountered in `value`. The caller is
    // responsible for ensuring that (a) `value` contains at least one
    // binder and (b) that binder is the one we want to use.
    let result = infcx.tcx.fold_regions(&value, &mut false, |r, current_depth| {
        match inv_skol_map.get(&r) {
            None => r,
            Some(br) => {
                // It is the responsibility of the caller to ensure
                // that each skolemized region appears within a
                // binder. In practice, this routine is only used by
                // trait checking, and all of the skolemized regions
                // appear inside predicates, which always have
                // binders, so this assert is satisfied.
                assert!(current_depth > 1);

                ty::ReLateBound(ty::DebruijnIndex::new(current_depth - 1), br.clone())
            }
        }
    });

    debug!("plug_leaks: result={:?}",
           result);

    result
}
