// Algorithm ported from Eigen, a lightweight C++ template library
// for linear algebra.
//
// Copyright (C) 2013 Gauthier Brun <brun.gauthier@gmail.com>
// Copyright (C) 2013 Nicolas Carre <nicolas.carre@ensimag.fr>
// Copyright (C) 2013 Jean Ceccato <jean.ceccato@ensimag.fr>
// Copyright (C) 2013 Pierre Zoppitelli <pierre.zoppitelli@ensimag.fr>
// Copyright (C) 2013 Jitse Niesen <jitse@maths.leeds.ac.uk>
// Copyright (C) 2014-2017 Gael Guennebaud <gael.guennebaud@inria.fr>
//
// Source Code Form is subject to the terms of the Mozilla
// Public License v. 2.0. If a copy of the MPL was not distributed
// with this file, You can obtain one at http://mozilla.org/MPL/2.0/.

use crate::jacobi::{jacobi_svd, Skip};
use coe::Coerce;
use core::{iter::zip, mem::swap};
use dyn_stack::{PodStack, SizeOverflow, StackReq};
use faer_core::{
    assert, group_helpers::SimdFor, jacobi::JacobiRotation, join_raw, temp_mat_req,
    temp_mat_uninit, temp_mat_zeroed, unzipped, zipped, ComplexField, Conj, Entity, MatMut, MatRef,
    Parallelism, RealField,
};
use reborrow::*;

#[allow(dead_code)]
fn bidiag_to_mat<E: RealField>(diag: &[E], subdiag: &[E]) -> faer_core::Mat<E> {
    let mut mat = faer_core::Mat::<E>::zeros(diag.len() + 1, diag.len());

    for (i, d) in diag.iter().enumerate() {
        mat.write(i, i, *d);
    }
    for (i, d) in subdiag.iter().enumerate() {
        mat.write(i + 1, i, *d);
    }

    mat
}

fn norm<E: RealField>(v: MatRef<'_, E>) -> E {
    faer_core::mul::inner_prod::inner_prod_with_conj(v, Conj::No, v, Conj::No).faer_sqrt()
}

fn compute_svd_of_m<E: RealField>(
    mut um: Option<MatMut<'_, E>>,
    mut vm: Option<MatMut<'_, E>>,
    diag: &mut [E],
    col0: &[E],
    outer_perm: &[usize],
    epsilon: E,
    _consider_zero_threshold: E,
    stack: PodStack<'_>,
) {
    let n = diag.len();

    diag[0] = E::faer_zero();
    let mut actual_n = n;
    while actual_n > 1 && diag[actual_n - 1] == E::faer_zero() {
        actual_n -= 1;
        assert!(col0[actual_n] == E::faer_zero());
    }

    let (perm, stack) = stack.collect(
        col0.iter()
            .take(actual_n)
            .map(|x| x.faer_abs())
            .enumerate()
            .filter(|(_, x)| *x != E::faer_zero())
            .map(|(i, _)| i),
    );
    let perm = &*perm;
    let (col0_perm, stack) = stack.collect(perm.iter().map(|&p| col0[p]));
    let (diag_perm, stack) = stack.collect(perm.iter().map(|&p| diag[p]));

    let (mut shifts, stack) = temp_mat_uninit::<E>(n, 1, stack);
    let shifts = shifts.as_mut();
    let (mut mus, stack) = temp_mat_uninit::<E>(n, 1, stack);
    let mus = mus.as_mut();
    let (mut singular_vals, stack) = temp_mat_uninit::<E>(n, 1, stack);
    let singular_vals = singular_vals.as_mut();
    let (mut zhat, stack) = temp_mat_uninit::<E>(n, 1, stack);
    let zhat = zhat.as_mut();

    let mut shifts = shifts.col_mut(0);
    let mut mus = mus.col_mut(0);
    let mut s = singular_vals.col_mut(0);
    let mut zhat = zhat.col_mut(0);

    compute_singular_values(
        shifts.rb_mut().as_2d_mut(),
        mus.rb_mut().as_2d_mut(),
        s.rb_mut().as_2d_mut(),
        diag,
        diag_perm,
        col0,
        col0_perm,
        epsilon,
    );
    perturb_col0(
        zhat.rb_mut().as_2d_mut(),
        col0,
        diag,
        perm,
        s.rb().as_2d(),
        shifts.rb().as_2d(),
        mus.rb().as_2d(),
    );

    let (col_perm, stack) = stack.make_with(actual_n, |i| i);
    let (col_perm_inv, _) = stack.make_with(actual_n, |i| i);

    for i in 0..actual_n - 1 {
        if s.read(i) > s.read(i + 1) {
            let si = s.read(i);
            let sj = s.read(i + 1);
            s.write(i, sj);
            s.write(i + 1, si);

            col_perm.swap(i, i + 1);
        }
    }
    for (i, p) in col_perm.iter().copied().enumerate() {
        col_perm_inv[p] = i;
    }

    compute_singular_vectors(
        um.rb_mut(),
        vm.rb_mut(),
        zhat.rb().as_2d(),
        diag,
        perm,
        outer_perm,
        col_perm_inv,
        actual_n,
        shifts.rb().as_2d(),
        mus.rb().as_2d(),
    );

    for (idx, diag) in diag[..actual_n].iter_mut().enumerate() {
        *diag = s.read(actual_n - idx - 1);
    }

    for (idx, diag) in diag[actual_n..n].iter_mut().enumerate() {
        *diag = s.read(actual_n + idx);
    }
}

#[inline(never)]
fn compute_singular_vectors<E: RealField>(
    mut um: Option<MatMut<E>>,
    mut vm: Option<MatMut<E>>,
    zhat: MatRef<E>,
    diag: &[E],
    perm: &[usize],
    outer_perm: &[usize],
    col_perm_inv: &[usize],
    actual_n: usize,
    shifts: MatRef<E>,
    mus: MatRef<E>,
) {
    let n = diag.len();

    for k in 0..n {
        let actual_k = if k >= actual_n {
            k
        } else {
            actual_n - col_perm_inv[k] - 1
        };
        let mut u = um.rb_mut().map(|u| u.col_mut(actual_k));
        let mut v = vm.rb_mut().map(|v| v.col_mut(actual_k));

        if zhat.read(k, 0) == E::faer_zero() {
            if let Some(mut u) = u.rb_mut() {
                u.write(outer_perm[k], E::faer_one());
            }
            if let Some(mut v) = v.rb_mut() {
                v.write(outer_perm[k], E::faer_one());
            }
            continue;
        }

        let mu = mus.read(k, 0);
        let shift = shifts.read(k, 0);

        assert_eq!(zhat.row_stride(), 1);

        if let Some(mut u) = u.rb_mut() {
            assert_eq!(u.row_stride(), 1);
            for &i in perm {
                u.write(
                    outer_perm[i],
                    zhat.read(i, 0)
                        .faer_div(diag[i].faer_sub(shift).faer_sub(mu))
                        .faer_div(diag[i].faer_add(shift.faer_add(mu))),
                );
            }
            u.write(n, E::faer_zero());
            let norm_inv = norm(u.rb().as_2d()).faer_inv();
            zipped!(u.rb_mut().as_2d_mut())
                .for_each(|unzipped!(mut x)| x.write(x.read().faer_mul(norm_inv)));
        }

        if let Some(mut v) = v {
            assert_eq!(v.row_stride(), 1);
            for &i in &perm[1..] {
                v.write(
                    outer_perm[i],
                    diag[i]
                        .faer_mul(zhat.read(i, 0))
                        .faer_div(diag[i].faer_sub(shift).faer_sub(mu))
                        .faer_div(diag[i].faer_add(shift.faer_add(mu))),
                );
            }
            v.write(outer_perm[0], E::faer_one().faer_neg());
            let norm_inv = norm(v.rb().as_2d()).faer_inv();
            zipped!(v.rb_mut().as_2d_mut())
                .for_each(|unzipped!(mut x)| x.write(x.read().faer_mul(norm_inv)));
        }
    }
    if let Some(mut um) = um {
        um.write(n, n, E::faer_one());
    }
}

fn perturb_col0<E: RealField>(
    mut zhat: MatMut<E>,
    col0: &[E],
    diag: &[E],
    perm: &[usize],
    s: MatRef<E>,
    shifts: MatRef<E>,
    mus: MatRef<E>,
) {
    let n = diag.len();
    let m = perm.len();
    if m == 0 {
        zipped!(zhat).for_each(|unzipped!(mut x)| x.write(E::faer_zero()));
        return;
    }

    let last_idx = perm[m - 1];
    for k in 0..n {
        if col0[k] == E::faer_zero() {
            zhat.write(k, 0, E::faer_zero());
            continue;
        }

        let dk = diag[k];
        let mut prod = (s.read(last_idx, 0).faer_add(dk)).faer_mul(
            mus.read(last_idx, 0)
                .faer_add(shifts.read(last_idx, 0).faer_sub(dk)),
        );

        for l in 0..m {
            let i = perm[l];
            if i == k {
                continue;
            }
            if i >= k && l == 0 {
                prod = E::faer_zero();
                break;
            }
            let j = if i < k {
                i
            } else if l > 0 {
                perm[l - 1]
            } else {
                i
            };

            let term = ((s.read(j, 0).faer_add(dk)).faer_div(diag[i].faer_add(dk))).faer_mul(
                (mus.read(j, 0).faer_add(shifts.read(j, 0).faer_sub(dk)))
                    .faer_div(diag[i].faer_sub(dk)),
            );
            prod = prod.faer_mul(term);
        }

        let tmp = prod.faer_sqrt();
        if col0[k] > E::faer_zero() {
            zhat.write(k, 0, tmp);
        } else {
            zhat.write(k, 0, tmp.faer_neg());
        }
    }
}

fn compute_singular_values<E: RealField>(
    shifts: MatMut<E>,
    mus: MatMut<E>,
    s: MatMut<E>,
    diag: &[E],
    diag_perm: &[E],
    col0: &[E],
    col0_perm: &[E],
    epsilon: E,
) {
    if coe::is_same::<f64, E>() {
        struct ImplF64<'a> {
            shifts: MatMut<'a, f64>,
            mus: MatMut<'a, f64>,
            s: MatMut<'a, f64>,
            diag: &'a [f64],
            diag_perm: &'a [f64],
            col0: &'a [f64],
            col0_perm: &'a [f64],
            epsilon: f64,
        }
        impl pulp::WithSimd for ImplF64<'_> {
            type Output = ();

            #[inline(always)]
            fn with_simd<S: pulp::Simd>(self, simd: S) -> Self::Output {
                let Self {
                    shifts,
                    mus,
                    s,
                    diag,
                    diag_perm,
                    col0,
                    col0_perm,
                    epsilon,
                } = self;
                compute_singular_values_generic::<f64>(
                    simd, shifts, mus, s, diag, diag_perm, col0, col0_perm, epsilon,
                )
            }
        }

        <f64 as ComplexField>::Simd::default().dispatch(ImplF64 {
            shifts: shifts.coerce(),
            mus: mus.coerce(),
            s: s.coerce(),
            diag: diag.coerce(),
            diag_perm: diag_perm.coerce(),
            col0: col0.coerce(),
            col0_perm: col0_perm.coerce(),
            epsilon: coe::coerce_static(epsilon),
        });
    } else if coe::is_same::<f32, E>() {
        struct ImplF32<'a> {
            shifts: MatMut<'a, f32>,
            mus: MatMut<'a, f32>,
            s: MatMut<'a, f32>,
            diag: &'a [f32],
            diag_perm: &'a [f32],
            col0: &'a [f32],
            col0_perm: &'a [f32],
            epsilon: f32,
        }
        impl pulp::WithSimd for ImplF32<'_> {
            type Output = ();

            #[inline(always)]
            fn with_simd<S: pulp::Simd>(self, simd: S) -> Self::Output {
                let Self {
                    shifts,
                    mus,
                    s,
                    diag,
                    diag_perm,
                    col0,
                    col0_perm,
                    epsilon,
                } = self;
                compute_singular_values_generic::<f32>(
                    simd, shifts, mus, s, diag, diag_perm, col0, col0_perm, epsilon,
                )
            }
        }

        <f64 as ComplexField>::Simd::default().dispatch(ImplF32 {
            shifts: shifts.coerce(),
            mus: mus.coerce(),
            s: s.coerce(),
            diag: diag.coerce(),
            diag_perm: diag_perm.coerce(),
            col0: col0.coerce(),
            col0_perm: col0_perm.coerce(),
            epsilon: coe::coerce_static(epsilon),
        });
    } else {
        compute_singular_values_generic(
            pulp::Scalar::new(),
            shifts,
            mus,
            s,
            diag,
            diag_perm,
            col0,
            col0_perm,
            epsilon,
        );
    }
}

#[inline(always)]
fn compute_singular_values_generic<E: RealField>(
    simd: impl pulp::Simd,
    mut shifts: MatMut<E>,
    mut mus: MatMut<E>,
    mut s: MatMut<E>,
    diag: &[E],
    diag_perm: &[E],
    col0: &[E],
    col0_perm: &[E],
    epsilon: E,
) {
    simd.vectorize(
        #[inline(always)]
        || {
            let n = diag.len();
            let mut actual_n = n;
            while actual_n > 1 && col0[actual_n - 1] == E::faer_zero() {
                actual_n -= 1;
            }
            let actual_n = actual_n;

            let two = E::faer_one().faer_add(E::faer_one());
            let eight = two
                .faer_scale_power_of_two(two)
                .faer_scale_power_of_two(two);
            let one_half = two.faer_inv();

            'kth_value: for k in 0..n {
                s.write(k, 0, E::faer_zero());
                shifts.write(k, 0, E::faer_zero());
                mus.write(k, 0, E::faer_zero());

                if col0[k] == E::faer_zero() || actual_n == 1 {
                    s.write(k, 0, if k == 0 { col0[0] } else { diag[k] });
                    shifts.write(k, 0, s.read(k, 0));
                    mus.write(k, 0, E::faer_zero());
                    continue 'kth_value;
                }

                let last_k = k == actual_n - 1;
                let left = diag[k];
                let right = if last_k {
                    let mut norm2 = E::faer_zero();
                    for &x in col0 {
                        norm2 = norm2.faer_add(x.faer_mul(x));
                    }
                    diag[actual_n - 1].faer_add(norm2.faer_sqrt())
                } else {
                    let mut l = k + 1;
                    while col0[l] == E::faer_zero() {
                        l += 1;
                    }
                    diag[l]
                };

                let mid = left.faer_add(right.faer_sub(left).faer_scale_power_of_two(one_half));
                let [mut f_mid, f_max, f_mid_left_shift, f_mid_right_shift] = secular_eq_multi_fast(
                    [
                        mid,
                        if last_k {
                            right.faer_sub(left)
                        } else {
                            (right.faer_sub(left)).faer_scale_power_of_two(one_half)
                        },
                        one_half.faer_mul(right.faer_sub(left)),
                        one_half.faer_mul(right.faer_sub(left)).faer_neg(),
                    ],
                    col0_perm,
                    diag_perm,
                    [E::faer_zero(), left, left, right],
                );

                let mut shift = if last_k || f_mid > E::faer_zero() {
                    left
                } else {
                    right
                };

                if !last_k {
                    if shift == left {
                        if f_mid_left_shift < E::faer_zero() {
                            shift = right;
                            f_mid = f_mid_right_shift;
                        }
                    } else if f_mid_right_shift > E::faer_zero() {
                        shift = left;
                        f_mid = f_mid_left_shift;
                    }
                }

                enum SecantError {
                    OutOfBounds,
                    PrecisionLimitReached,
                }

                let secant = {
                    #[inline(always)]
                    |mut mu_cur: E, mut mu_prev: E, mut f_cur: E, mut f_prev: E| {
                        if f_prev.faer_abs() < f_cur.faer_abs() {
                            swap(&mut f_prev, &mut f_cur);
                            swap(&mut mu_prev, &mut mu_cur);
                        }

                        let mut left_candidate = None;
                        let mut right_candidate = None;

                        let mut use_bisection = false;
                        let same_sign = f_prev.faer_mul(f_cur) > E::faer_zero();
                        if !same_sign {
                            let (min, max) = if mu_cur < mu_prev {
                                (mu_cur, mu_prev)
                            } else {
                                (mu_prev, mu_cur)
                            };
                            left_candidate = Some(min);
                            right_candidate = Some(max);
                        }

                        let mut err = SecantError::PrecisionLimitReached;

                        while f_cur != E::faer_zero()
                            && ((mu_cur.faer_sub(mu_prev)).faer_abs()
                                > eight.faer_mul(epsilon).faer_mul(
                                    if mu_cur.faer_abs() > mu_prev.faer_abs() {
                                        mu_cur.faer_abs()
                                    } else {
                                        mu_prev.faer_abs()
                                    },
                                ))
                            && ((f_cur.faer_sub(f_prev)).faer_abs() > epsilon)
                            && !use_bisection
                        {
                            // rational interpolation: fit a function of the form a / mu + b through
                            // the two previous iterates and use its
                            // zero to compute the next iterate
                            let a = (f_cur.faer_sub(f_prev))
                                .faer_mul(mu_prev.faer_mul(mu_cur))
                                .faer_div(mu_prev.faer_sub(mu_cur));
                            let b = f_cur.faer_sub(a.faer_div(mu_cur));
                            let mu_zero = a.faer_div(b).faer_neg();
                            let f_zero = secular_eq(mu_zero, col0_perm, diag_perm, shift);

                            if f_zero < E::faer_zero() {
                                left_candidate = Some(mu_zero);
                            } else {
                                right_candidate = Some(mu_zero);
                            }

                            mu_prev = mu_cur;
                            f_prev = f_cur;
                            mu_cur = mu_zero;
                            f_cur = f_zero;

                            if shift == left
                                && (mu_cur < E::faer_zero() || mu_cur > (right.faer_sub(left)))
                            {
                                err = SecantError::OutOfBounds;
                                use_bisection = true;
                            }
                            if shift == right
                                && (mu_cur < (right.faer_sub(left)).faer_neg()
                                    || mu_cur > E::faer_zero())
                            {
                                err = SecantError::OutOfBounds;
                                use_bisection = true;
                            }
                            if f_cur.faer_abs() > f_prev.faer_abs() {
                                // find mu such that a / mu + b = -k * f_zero
                                // a / mu = -f_zero - b
                                // mu = -a / (f_zero + b)
                                let mut k = E::faer_one();
                                for _ in 0..4 {
                                    let mu_opposite =
                                        a.faer_neg().faer_div(k.faer_mul(f_zero).faer_add(b));
                                    let f_opposite =
                                        secular_eq(mu_opposite, col0_perm, diag_perm, shift);
                                    if f_zero < E::faer_zero() && f_opposite >= E::faer_zero() {
                                        // this will be our right candidate
                                        right_candidate = Some(mu_opposite);
                                        break;
                                    } else if f_zero > E::faer_zero()
                                        && f_opposite <= E::faer_zero()
                                    {
                                        // this will be our left candidate
                                        left_candidate = Some(mu_opposite);
                                        break;
                                    }
                                    k = k.faer_scale_power_of_two(two);
                                }
                                use_bisection = true;
                            }
                        }
                        (use_bisection, mu_cur, left_candidate, right_candidate, err)
                    }
                };

                let (mut left_shifted, mut f_left, mut right_shifted, mut f_right) =
                    if shift == left {
                        (
                            E::faer_zero(),
                            E::faer_zero().faer_inv().faer_neg(),
                            if last_k {
                                right.faer_sub(left)
                            } else {
                                (right.faer_sub(left)).faer_mul(one_half)
                            },
                            if last_k { f_max } else { f_mid },
                        )
                    } else {
                        (
                            (right.faer_sub(left))
                                .faer_neg()
                                .faer_scale_power_of_two(one_half),
                            f_mid,
                            E::faer_zero(),
                            E::faer_zero().faer_inv(),
                        )
                    };

                assert!(
                    PartialOrd::partial_cmp(&f_left, &E::faer_zero())
                        != Some(core::cmp::Ordering::Greater)
                );
                assert!(
                    PartialOrd::partial_cmp(&f_right, &E::faer_zero())
                        != Some(core::cmp::Ordering::Less)
                );

                let mut iteration_count = 0;
                let mut f_prev = f_mid;
                // try to find non zero starting bounds

                let half0 = one_half;
                let half1 = half0.faer_scale_power_of_two(half0);
                let half2 = half1.faer_scale_power_of_two(half1);
                let half3 = half2.faer_scale_power_of_two(half2);
                let half4 = half3.faer_scale_power_of_two(half3);
                let half5 = half4.faer_scale_power_of_two(half4);
                let half6 = half5.faer_scale_power_of_two(half5);
                let half7 = half6.faer_scale_power_of_two(half6);

                let mu_values = if shift == left {
                    [
                        right_shifted.faer_scale_power_of_two(half7),
                        right_shifted.faer_scale_power_of_two(half6),
                        right_shifted.faer_scale_power_of_two(half5),
                        right_shifted.faer_scale_power_of_two(half4),
                        right_shifted.faer_scale_power_of_two(half3),
                        right_shifted.faer_scale_power_of_two(half2),
                        right_shifted.faer_scale_power_of_two(half1),
                        right_shifted.faer_scale_power_of_two(half0),
                    ]
                } else {
                    [
                        left_shifted.faer_scale_power_of_two(half7),
                        left_shifted.faer_scale_power_of_two(half6),
                        left_shifted.faer_scale_power_of_two(half5),
                        left_shifted.faer_scale_power_of_two(half4),
                        left_shifted.faer_scale_power_of_two(half3),
                        left_shifted.faer_scale_power_of_two(half2),
                        left_shifted.faer_scale_power_of_two(half1),
                        left_shifted.faer_scale_power_of_two(half0),
                    ]
                };
                let f_values =
                    secular_eq_multi_fast(mu_values, col0_perm, diag_perm, [(); 8].map(|_| shift));

                if shift == left {
                    let mut i = 0;
                    for (mu, f) in zip(mu_values, f_values) {
                        if f < E::faer_zero() {
                            left_shifted = mu;
                            f_left = f;
                            i += 1;
                        }
                    }
                    if i < f_values.len() {
                        right_shifted = mu_values[i];
                        f_right = f_values[i];
                    }
                } else {
                    let mut i = 0;
                    for (mu, f) in zip(mu_values, f_values) {
                        if f > E::faer_zero() {
                            right_shifted = mu;
                            f_right = f;
                            i += 1;
                        }
                    }
                    if i < f_values.len() {
                        left_shifted = mu_values[i];
                        f_left = f_values[i];
                    }
                }

                // try bisection just to get a good guess for secant
                while right_shifted.faer_sub(left_shifted)
                    > two.faer_mul(epsilon).faer_mul(
                        if left_shifted.faer_abs() > right_shifted.faer_abs() {
                            left_shifted.faer_abs()
                        } else {
                            right_shifted.faer_abs()
                        },
                    )
                {
                    let mid_shifted_arithmetic =
                        (left_shifted.faer_add(right_shifted)).faer_scale_power_of_two(one_half);
                    let mut mid_shifted_geometric = left_shifted
                        .faer_abs()
                        .faer_sqrt()
                        .faer_mul(right_shifted.faer_abs().faer_sqrt());
                    if left_shifted < E::faer_zero() {
                        mid_shifted_geometric = mid_shifted_geometric.faer_neg();
                    }
                    let mid_shifted = if mid_shifted_geometric == E::faer_zero() {
                        mid_shifted_arithmetic
                    } else {
                        mid_shifted_geometric
                    };
                    let f_mid = secular_eq(mid_shifted, col0_perm, diag_perm, shift);

                    if f_mid == E::faer_zero() {
                        s.write(k, 0, shift.faer_add(mid_shifted));
                        shifts.write(k, 0, shift);
                        mus.write(k, 0, mid_shifted);
                        continue 'kth_value;
                    } else if f_mid > E::faer_zero() {
                        right_shifted = mid_shifted;
                        f_prev = f_right;
                        f_right = f_mid;
                    } else {
                        left_shifted = mid_shifted;
                        f_prev = f_left;
                        f_left = f_mid;
                    }

                    if iteration_count == 4 {
                        break;
                    }

                    iteration_count += 1;
                }

                // try secant with the guess from bisection
                let args = if left_shifted == E::faer_zero() {
                    (
                        right_shifted.faer_add(right_shifted),
                        right_shifted,
                        f_prev,
                        f_right,
                    )
                } else if right_shifted == E::faer_zero() {
                    (
                        left_shifted.faer_add(left_shifted),
                        left_shifted,
                        f_prev,
                        f_left,
                    )
                } else {
                    (left_shifted, right_shifted, f_left, f_right)
                };

                let (use_bisection, mut mu_cur, left_candidate, right_candidate, _err) =
                    secant(args.0, args.1, args.2, args.3);

                match (left_candidate, right_candidate) {
                    (Some(left), Some(right)) if left < right => {
                        if left > left_shifted {
                            left_shifted = left;
                        }
                        if right < right_shifted {
                            right_shifted = right;
                        }
                    }
                    _ => (),
                }

                // secant failed, use bisection again
                if use_bisection {
                    while (right_shifted.faer_sub(left_shifted))
                        > two.faer_mul(epsilon).faer_mul(
                            if left_shifted.faer_abs() > right_shifted.faer_abs() {
                                left_shifted.faer_abs()
                            } else {
                                right_shifted.faer_abs()
                            },
                        )
                    {
                        let mid_shifted = (left_shifted.faer_add(right_shifted))
                            .faer_scale_power_of_two(one_half);
                        let f_mid = secular_eq(mid_shifted, col0_perm, diag_perm, shift);

                        if f_mid == E::faer_zero() {
                            break;
                        } else if f_mid > E::faer_zero() {
                            right_shifted = mid_shifted;
                        } else {
                            left_shifted = mid_shifted;
                        }
                    }

                    mu_cur = (left_shifted.faer_add(right_shifted)).faer_mul(one_half);
                }

                s.write(k, 0, shift.faer_add(mu_cur));
                shifts.write(k, 0, shift);
                mus.write(k, 0, mu_cur);
            }
        },
    );
}

#[inline(always)]
fn secular_eq_multi_fast<const N: usize, E: RealField>(
    mu: [E; N],
    col0_perm: &[E],
    diag_perm: &[E],
    shift: [E; N],
) -> [E; N] {
    let mut res0 = [(); N].map(|_| E::faer_one());
    for (c0, d0) in col0_perm.iter().cloned().zip(diag_perm.iter().cloned()) {
        for ((res0, mu), shift) in res0
            .iter_mut()
            .zip(mu.iter().cloned())
            .zip(shift.iter().cloned())
        {
            *res0 = (*res0).faer_add((c0.faer_mul(c0)).faer_div(
                (d0.faer_sub(shift).faer_sub(mu)).faer_mul(d0.faer_add(shift).faer_add(mu)),
            ));
        }
    }
    res0
}

#[inline(always)]
fn secular_eq<E: RealField>(mu: E, col0_perm: &[E], diag_perm: &[E], shift: E) -> E {
    let mut res0 = E::faer_one();
    let mut res1 = E::faer_zero();
    let mut res2 = E::faer_zero();
    let mut res3 = E::faer_zero();
    let mut res4 = E::faer_zero();
    let mut res5 = E::faer_zero();
    let mut res6 = E::faer_zero();
    let mut res7 = E::faer_zero();

    let (col0_head, col0_perm) = pulp::as_arrays::<8, _>(col0_perm);
    let (diag_head, diag_perm) = pulp::as_arrays::<8, _>(diag_perm);
    for ([c0, c1, c2, c3, c4, c5, c6, c7], [d0, d1, d2, d3, d4, d5, d6, d7]) in
        col0_head.iter().zip(diag_head)
    {
        res0 = res0.faer_add(
            (c0.faer_div(d0.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c0.faer_div(d0.faer_add(shift).faer_add(mu))),
        );
        res1 = res1.faer_add(
            (c1.faer_div(d1.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c1.faer_div(d1.faer_add(shift).faer_add(mu))),
        );
        res2 = res2.faer_add(
            (c2.faer_div(d2.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c2.faer_div(d2.faer_add(shift).faer_add(mu))),
        );
        res3 = res3.faer_add(
            (c3.faer_div(d3.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c3.faer_div(d3.faer_add(shift).faer_add(mu))),
        );
        res4 = res4.faer_add(
            (c4.faer_div(d4.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c4.faer_div(d4.faer_add(shift).faer_add(mu))),
        );
        res5 = res5.faer_add(
            (c5.faer_div(d5.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c5.faer_div(d5.faer_add(shift).faer_add(mu))),
        );
        res6 = res6.faer_add(
            (c6.faer_div(d6.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c6.faer_div(d6.faer_add(shift).faer_add(mu))),
        );
        res7 = res7.faer_add(
            (c7.faer_div(d7.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c7.faer_div(d7.faer_add(shift).faer_add(mu))),
        );
    }

    let (col0_head, col0_perm) = pulp::as_arrays::<4, _>(col0_perm);
    let (diag_head, diag_perm) = pulp::as_arrays::<4, _>(diag_perm);
    for ([c0, c1, c2, c3], [d0, d1, d2, d3]) in col0_head.iter().zip(diag_head) {
        res0 = res0.faer_add(
            (c0.faer_div(d0.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c0.faer_div(d0.faer_add(shift).faer_add(mu))),
        );
        res1 = res1.faer_add(
            (c1.faer_div(d1.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c1.faer_div(d1.faer_add(shift).faer_add(mu))),
        );
        res2 = res2.faer_add(
            (c2.faer_div(d2.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c2.faer_div(d2.faer_add(shift).faer_add(mu))),
        );
        res3 = res3.faer_add(
            (c3.faer_div(d3.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c3.faer_div(d3.faer_add(shift).faer_add(mu))),
        );
    }

    let (col0_head, col0_perm) = pulp::as_arrays::<2, _>(col0_perm);
    let (diag_head, diag_perm) = pulp::as_arrays::<2, _>(diag_perm);
    for ([c0, c1], [d0, d1]) in col0_head.iter().zip(diag_head) {
        res0 = res0.faer_add(
            (c0.faer_div(d0.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c0.faer_div(d0.faer_add(shift).faer_add(mu))),
        );
        res1 = res1.faer_add(
            (c1.faer_div(d1.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c1.faer_div(d1.faer_add(shift).faer_add(mu))),
        );
    }

    for (c0, d0) in col0_perm.iter().zip(diag_perm) {
        res0 = res0.faer_add(
            (c0.faer_div(d0.faer_sub(shift).faer_sub(mu)))
                .faer_mul(c0.faer_div(d0.faer_add(shift).faer_add(mu))),
        );
    }

    ((res0.faer_add(res1)).faer_add(res2.faer_add(res3)))
        .faer_add((res4.faer_add(res5)).faer_add(res6.faer_add(res7)))
}

fn deflate<E: RealField>(
    diag: &mut [E],
    col0: &mut [E],
    jacobi_coeffs: &mut [JacobiRotation<E>],
    jacobi_indices: &mut [usize],
    mut u: MatMut<'_, E>,
    mut v: Option<MatMut<'_, E>>,
    transpositions: &mut [usize],
    perm: &mut [usize],
    k: usize,
    epsilon: E,
    consider_zero_threshold: E,
    stack: PodStack<'_>,
) -> (usize, usize) {
    let n = diag.len();
    let mut jacobi_0i = 0;
    let mut jacobi_ij = 0;

    let mut max_diag = E::faer_zero();
    let mut max_col0 = E::faer_zero();
    for d in diag[1..].iter() {
        max_diag = if d.faer_abs() > max_diag {
            d.faer_abs()
        } else {
            max_diag
        };
    }
    for d in col0.iter() {
        max_col0 = if d.faer_abs() > max_col0 {
            d.faer_abs()
        } else {
            max_col0
        };
    }

    let epsilon_strict = epsilon.faer_mul(max_diag);
    let epsilon_strict = if epsilon_strict > consider_zero_threshold {
        &epsilon_strict
    } else {
        &consider_zero_threshold
    };

    let two = E::faer_one().faer_add(E::faer_one());
    let eight = two
        .faer_scale_power_of_two(two)
        .faer_scale_power_of_two(two);
    let epsilon_coarse = eight.faer_mul(epsilon).faer_mul(if max_diag > max_col0 {
        max_diag
    } else {
        max_col0
    });

    // condition 4.1
    if diag[0] < epsilon_coarse {
        diag[0] = epsilon_coarse;
        col0[0] = epsilon_coarse;
    }

    // condition 4.2
    for x in &mut col0[1..] {
        if x.faer_abs() < *epsilon_strict {
            *x = E::faer_zero();
        }
    }

    // condition 4.3
    for i in 1..n {
        if diag[i] < epsilon_coarse {
            if let Some(rot) = deflation43(diag, col0, u.rb_mut(), i) {
                jacobi_coeffs[jacobi_0i] = rot;
                jacobi_indices[jacobi_0i] = i;
                jacobi_0i += 1;
            }
        }
    }

    let mut total_deflation = true;
    for c in col0[1..].iter() {
        if PartialOrd::partial_cmp(&c.faer_abs(), &consider_zero_threshold)
            != Some(core::cmp::Ordering::Less)
        {
            total_deflation = false;
            break;
        }
    }

    let mut p = 1;

    for (d, i) in diag[1..].iter().zip(1..n) {
        if d.faer_abs() < consider_zero_threshold {
            perm[p] = i;
            p += 1;
        }
    }

    let mut i = 1;
    let mut j = k + 1;

    for p in &mut perm[p..] {
        if i > k {
            *p = j;
            j += 1;
        } else if j >= n {
            *p = i;
            i += 1;
        } else if diag[i] < diag[j] {
            *p = j;
            j += 1;
        } else {
            *p = i;
            i += 1;
        }
    }

    if total_deflation {
        for i in 1..n {
            let pi = perm[i];
            if diag[pi].faer_abs() < consider_zero_threshold || diag[pi] > diag[0] {
                perm[i - 1] = perm[i];
            } else {
                perm[i - 1] = 0;
                break;
            }
        }
    }

    let (real_ind, stack) = stack.make_with(n, |i| i);
    let (real_col, _) = stack.make_with(n, |i| i);

    for i in (if total_deflation { 0 } else { 1 })..n {
        let pi = perm[n - (if total_deflation { i + 1 } else { i })];
        let j = real_col[pi];

        diag.swap(i, j);

        if i != 0 && j != 0 {
            col0.swap(i, j);
        }

        transpositions[i] = j;

        let real_i = real_ind[i];
        real_col[real_i] = j;
        real_col[pi] = i;
        real_ind[j] = real_i;
        real_ind[i] = pi;
    }
    col0[0] = diag[0];
    for (i, p) in perm.iter_mut().enumerate() {
        *p = i;
    }
    for (i, j) in transpositions.iter().copied().enumerate() {
        perm.swap(i, j);
    }

    // condition 4.4
    let mut i = n - 1;
    while i > 0
        && (diag[i].faer_abs() < consider_zero_threshold
            || col0[i].faer_abs() < consider_zero_threshold)
    {
        i -= 1;
    }
    while i > 1 {
        if diag[i].faer_sub(diag[i - 1]) < *epsilon_strict {
            if let Some(rot) = deflation44(diag, col0, u.rb_mut(), v.rb_mut(), i - 1, i) {
                jacobi_coeffs[jacobi_0i + jacobi_ij] = rot;
                jacobi_indices[jacobi_0i + jacobi_ij] = i;
                jacobi_ij += 1;
            }
        }
        i -= 1;
    }

    (jacobi_0i, jacobi_ij)
}

fn deflation43<E: RealField>(
    diag: &mut [E],
    col0: &mut [E],
    _u: MatMut<E>,
    i: usize,
) -> Option<JacobiRotation<E>> {
    let c = col0[0];
    let s = col0[i];
    let r = ((c.faer_mul(c)).faer_add(s.faer_mul(s))).faer_sqrt();
    if r == E::faer_zero() {
        diag[i] = E::faer_zero();
        return None;
    }

    col0[0] = r;
    diag[0] = r;
    col0[i] = E::faer_zero();
    diag[i] = E::faer_zero();

    let rot = JacobiRotation {
        c: c.faer_div(r),
        s: s.faer_neg().faer_div(r),
    };
    Some(rot)
}

fn deflation44<E: RealField>(
    diag: &mut [E],
    col0: &mut [E],
    _u: MatMut<E>,
    _v: Option<MatMut<E>>,
    i: usize,
    j: usize,
) -> Option<JacobiRotation<E>> {
    let c = col0[i];
    let s = col0[j];
    let r = ((c.faer_mul(c)).faer_add(s.faer_mul(s))).faer_sqrt();
    if r == E::faer_zero() {
        diag[i] = diag[j];
        return None;
    }

    let c = c.faer_div(r);
    let s = s.faer_neg().faer_div(r);
    col0[i] = r;
    diag[j] = diag[i];
    col0[j] = E::faer_zero();

    let rot = JacobiRotation { c, s };
    Some(rot)
}

fn bidiag_svd_qr_algorithm_impl<E: RealField>(
    diag: &mut [E],
    subdiag: &mut [E],
    mut u: Option<MatMut<'_, E>>,
    mut v: Option<MatMut<'_, E>>,
    epsilon: E,
    consider_zero_threshold: E,
) {
    let n = diag.len();
    let max_iter = 30usize.saturating_mul(n).saturating_mul(n);

    let epsilon = epsilon.faer_scale_real(E::faer_from_f64(128.0));

    if let Some(mut u) = u.rb_mut() {
        u.fill_zero();
        u.diagonal_mut().column_vector_mut().fill(E::faer_one());
    }
    if let Some(mut v) = v.rb_mut() {
        v.fill_zero();
        v.diagonal_mut().column_vector_mut().fill(E::faer_one());
    }

    u = u.map(|u| u.submatrix_mut(0, 0, n, n));
    v = v.map(|v| v.submatrix_mut(0, 0, n, n));

    let mut max_val = E::faer_zero();

    for x in &*diag {
        let val = x.faer_abs();
        if val > max_val {
            max_val = val;
        }
    }
    for x in &*subdiag {
        let val = x.faer_abs();
        if val > max_val {
            max_val = val;
        }
    }

    let max_val = E::faer_one();

    if max_val == E::faer_zero() {
        return;
    }

    for x in &mut *diag {
        *x = (*x).faer_div(max_val);
    }
    for x in &mut *subdiag {
        *x = (*x).faer_div(max_val);
    }

    struct Impl<'a, E: Entity> {
        epsilon: E,
        consider_zero_threshold: E,
        max_iter: usize,
        diag: &'a mut [E],
        subdiag: &'a mut [E],
        u: Option<MatMut<'a, E>>,
        v: Option<MatMut<'a, E>>,
    }

    impl<E: RealField> pulp::WithSimd for Impl<'_, E> {
        type Output = ();

        #[inline(always)]
        fn with_simd<S: pulp::Simd>(self, simd: S) -> Self::Output {
            let Self {
                epsilon,
                consider_zero_threshold,
                max_iter,
                diag,
                subdiag,
                mut u,
                mut v,
            } = self;
            let n = diag.len();
            let arch = E::Simd::default();

            for iter in 0..max_iter {
                let _ = iter;
                for i in 0..n - 1 {
                    if subdiag[i].faer_abs()
                        <= epsilon.faer_mul(diag[i].faer_abs().faer_add(diag[i + 1].faer_abs()))
                        || subdiag[i].faer_abs() <= epsilon
                    {
                        subdiag[i] = E::faer_zero();
                    }
                }
                for i in 0..n {
                    if diag[i].faer_abs() <= epsilon {
                        diag[i] = E::faer_zero();
                    }
                }

                let mut end = n;
                while end > 1 && subdiag[end - 2].faer_abs() <= consider_zero_threshold.faer_sqrt()
                {
                    end -= 1;
                }

                if end == 1 {
                    break;
                }

                let mut start = end - 1;
                while start > 0 && subdiag[start - 1] != E::faer_zero() {
                    start -= 1;
                }

                let mut found_zero_diag = false;
                for i in start..end - 1 {
                    if diag[i] == E::faer_zero() {
                        found_zero_diag = true;
                        let mut val = subdiag[i];
                        subdiag[i] = E::faer_zero();
                        for j in i + 1..end {
                            let rot = JacobiRotation::make_givens(diag[j], val);
                            diag[j] = rot
                                .c
                                .faer_mul(diag[j])
                                .faer_sub(rot.s.faer_mul(val))
                                .faer_abs();

                            if j < end - 1 {
                                (val, subdiag[j]) = (
                                    rot.s.faer_mul(subdiag[j]).faer_neg(),
                                    rot.c.faer_mul(subdiag[j]),
                                );
                            }

                            if let Some(v) = v.rb_mut() {
                                unsafe {
                                    rot.apply_on_the_right_in_place_arch(
                                        arch,
                                        v.rb().col(i).as_2d().const_cast(),
                                        v.rb().col(j).as_2d().const_cast(),
                                    );
                                }
                            }
                        }
                    }
                }

                if found_zero_diag {
                    continue;
                }

                let t00 = if end - start == 2 {
                    diag[end - 2].faer_abs2()
                } else {
                    diag[end - 2]
                        .faer_abs2()
                        .faer_add(subdiag[end - 3].faer_abs2())
                };
                let t11 = diag[end - 1]
                    .faer_abs2()
                    .faer_add(subdiag[end - 2].faer_abs2());
                let t01 = diag[end - 2].faer_mul(subdiag[end - 2]);

                let mu;
                if false {
                    let delta = E::faer_sub(
                        t00.faer_add(t11).faer_abs2(),
                        t00.faer_mul(t11)
                            .faer_sub(t01.faer_abs2())
                            .faer_scale_power_of_two(E::faer_from_f64(4.0)),
                    );

                    mu = if delta > E::faer_zero() {
                        let lambda0 = t00
                            .faer_add(t11)
                            .faer_add(delta.faer_sqrt())
                            .faer_scale_power_of_two(E::faer_from_f64(0.5));
                        let lambda1 = t00
                            .faer_add(t11)
                            .faer_sub(delta.faer_sqrt())
                            .faer_scale_power_of_two(E::faer_from_f64(0.5));

                        if lambda0.faer_sub(t11).faer_abs() < lambda1.faer_sub(t11).faer_abs() {
                            lambda0
                        } else {
                            lambda1
                        }
                    } else {
                        t11
                    };
                } else {
                    let t01_2 = t01.faer_abs2();
                    if t01_2 > consider_zero_threshold {
                        let d = (t00.faer_sub(t11)).faer_mul(E::faer_from_f64(0.5));
                        let mut delta = d.faer_abs2().faer_add(t01_2).faer_sqrt();
                        if d < E::faer_zero() {
                            delta = delta.faer_neg();
                        }

                        mu = t11.faer_sub(t01_2.faer_div(d.faer_add(delta)));
                    } else {
                        mu = t11
                    }
                }

                let mut y = diag[start].faer_abs2().faer_sub(mu);
                let mut z = diag[start].faer_mul(subdiag[start]);

                let simde = SimdFor::<E, S>::new(simd);
                let u_offset = simde.align_offset_ptr(
                    u.rb()
                        .map(|mat| mat.as_ptr())
                        .unwrap_or(E::faer_map(E::UNIT, |()| core::ptr::null())),
                    diag.len(),
                );
                let v_offset = simde.align_offset_ptr(
                    v.rb()
                        .map(|mat| mat.as_ptr())
                        .unwrap_or(E::faer_map(E::UNIT, |()| core::ptr::null())),
                    diag.len(),
                );

                for k in start..end - 1 {
                    let rot = JacobiRotation::make_givens(y, z);
                    if k > start {
                        subdiag[k - 1] = rot.c.faer_mul(y).faer_sub(rot.s.faer_mul(z)).faer_abs();
                    }

                    let mut diag_k = diag[k];

                    (diag_k, subdiag[k]) = (
                        simde.scalar_mul_add_e(
                            rot.c,
                            diag_k,
                            simde.scalar_mul(rot.s.faer_neg(), subdiag[k]),
                        ),
                        simde.scalar_mul_add_e(rot.s, diag_k, simde.scalar_mul(rot.c, subdiag[k])),
                    );

                    y = diag_k;
                    (z, diag[k + 1]) = (
                        simde.scalar_mul(rot.s.faer_neg(), diag[k + 1]),
                        simde.scalar_mul(rot.c, diag[k + 1]),
                    );

                    if let Some(u) = u.rb_mut() {
                        unsafe {
                            rot.apply_on_the_right_in_place_with_simd_and_offset(
                                simd,
                                u_offset,
                                u.rb().col(k).as_2d().const_cast(),
                                u.rb().col(k + 1).as_2d().const_cast(),
                            );
                        }
                    }

                    let rot = JacobiRotation::make_givens(y, z);

                    diag_k = rot.c.faer_mul(y).faer_sub(rot.s.faer_mul(z)).faer_abs();
                    diag[k] = diag_k;
                    (subdiag[k], diag[k + 1]) = (
                        simde.scalar_mul_add_e(
                            rot.c,
                            subdiag[k],
                            simde.scalar_mul(rot.s.faer_neg(), diag[k + 1]),
                        ),
                        simde.scalar_mul_add_e(
                            rot.s,
                            subdiag[k],
                            simde.scalar_mul(rot.c, diag[k + 1]),
                        ),
                    );

                    if k < end - 2 {
                        y = subdiag[k];
                        (z, subdiag[k + 1]) = (
                            simde.scalar_mul(rot.s.faer_neg(), subdiag[k + 1]),
                            simde.scalar_mul(rot.c, subdiag[k + 1]),
                        );
                    }

                    if let Some(v) = v.rb_mut() {
                        unsafe {
                            rot.apply_on_the_right_in_place_with_simd_and_offset(
                                simd,
                                v_offset,
                                v.rb().col(k).as_2d().const_cast(),
                                v.rb().col(k + 1).as_2d().const_cast(),
                            );
                        }
                    }
                }
            }
        }
    }

    use faer_entity::SimdCtx;
    E::Simd::default().dispatch(Impl {
        epsilon,
        consider_zero_threshold,
        max_iter,
        diag,
        subdiag,
        u: u.rb_mut(),
        v: v.rb_mut(),
    });

    for (j, d) in diag.iter_mut().enumerate() {
        if *d < E::faer_zero() {
            *d = d.faer_neg();
            if let Some(mut v) = v.rb_mut() {
                for i in 0..n {
                    v.write(i, j, v.read(i, j).faer_neg());
                }
            }
        }
    }

    for k in 0..n {
        let mut max = E::faer_zero();
        let mut max_idx = k;
        for kk in k..n {
            if diag[kk] > max {
                max = diag[kk];
                max_idx = kk;
            }
        }

        if k != max_idx {
            diag.swap(k, max_idx);
            if let Some(u) = u.rb_mut() {
                faer_core::permutation::swap_cols(u, k, max_idx);
            }
            if let Some(v) = v.rb_mut() {
                faer_core::permutation::swap_cols(v, k, max_idx);
            }
        }
    }

    for x in &mut *diag {
        *x = (*x).faer_mul(max_val);
    }
}

/// svd of bidiagonal lower matrix of shape (n + 1, n), with the last row being all zeros
pub fn compute_bidiag_real_svd<E: RealField>(
    diag: &mut [E],
    subdiag: &mut [E],
    mut u: Option<MatMut<'_, E>>,
    v: Option<MatMut<'_, E>>,
    jacobi_fallback_threshold: usize,
    bidiag_qr_fallback_threshold: usize,
    epsilon: E,
    consider_zero_threshold: E,
    parallelism: Parallelism,
    stack: PodStack<'_>,
) {
    let n = diag.len();

    if n <= jacobi_fallback_threshold {
        let (mut s, _) = temp_mat_zeroed::<E>(n, n, stack);
        let mut s = s.as_mut();

        for i in 0..n {
            s.write(i, i, diag[i]);
            if i + 1 < n {
                s.write(i + 1, i, subdiag[i]);
            }
        }

        jacobi_svd(
            s.rb_mut(),
            u.rb_mut().map(|u| u.submatrix_mut(0, 0, n, n)),
            v,
            Skip::None,
            epsilon,
            consider_zero_threshold,
        );

        for (i, diag) in diag.iter_mut().enumerate() {
            *diag = s.read(i, i);
        }
        if let Some(mut u) = u {
            zipped!(u.rb_mut().row_mut(n).as_2d_mut())
                .for_each(|unzipped!(mut x)| x.write(E::faer_zero()));
            zipped!(u.rb_mut().col_mut(n).as_2d_mut())
                .for_each(|unzipped!(mut x)| x.write(E::faer_zero()));
            u.write(n, n, E::faer_one());
        }
    } else if n <= bidiag_qr_fallback_threshold {
        bidiag_svd_qr_algorithm_impl(diag, subdiag, u, v, epsilon, consider_zero_threshold);
    } else {
        match u {
            Some(u) => bidiag_svd_impl(
                diag,
                subdiag,
                u,
                v,
                true,
                jacobi_fallback_threshold,
                epsilon,
                consider_zero_threshold,
                parallelism,
                stack,
            ),
            None => {
                let (mut u, stack) = temp_mat_uninit::<E>(2, n + 1, stack);
                let u = u.as_mut();
                bidiag_svd_impl(
                    diag,
                    subdiag,
                    u,
                    v,
                    false,
                    jacobi_fallback_threshold,
                    epsilon,
                    consider_zero_threshold,
                    parallelism,
                    stack,
                );
            }
        }
    }
}

/// svd of bidiagonal lower matrix
fn bidiag_svd_impl<E: RealField>(
    diag: &mut [E],
    subdiag: &mut [E],
    mut u: MatMut<'_, E>,
    mut v: Option<MatMut<'_, E>>,
    fill_u: bool,
    jacobi_fallback_threshold: usize,
    epsilon: E,
    consider_zero_threshold: E,
    parallelism: Parallelism,
    mut stack: PodStack<'_>,
) {
    let n = diag.len();

    let mut max_val = E::faer_zero();

    for x in &*diag {
        let val = x.faer_abs();
        if val > max_val {
            max_val = val;
        }
    }
    for x in &*subdiag {
        let val = x.faer_abs();
        if val > max_val {
            max_val = val;
        }
    }

    if max_val == E::faer_zero() {
        u.fill_zero();
        if u.nrows() == n + 1 {
            u.diagonal_mut().column_vector_mut().fill(E::faer_one());
        } else {
            u.write(0, 0, E::faer_one());
            u.write(1, n, E::faer_one());
        }
        if let Some(mut v) = v {
            v.fill_zero();
            v.diagonal_mut().column_vector_mut().fill(E::faer_one());
        };
        return;
    }

    for x in &mut *diag {
        *x = (*x).faer_div(max_val);
    }
    for x in &mut *subdiag {
        *x = (*x).faer_div(max_val);
    }

    assert!(subdiag.len() == n);
    assert!(n > jacobi_fallback_threshold);

    let k = n / 2;
    let rem = n - 1 - k;

    let (d1, alpha_d2) = diag.split_at_mut(k);
    let (sub_d1, beta_sub_d2) = subdiag.split_at_mut(k);
    let (alpha, d2) = alpha_d2.split_first_mut().unwrap();
    let (beta, sub_d2) = beta_sub_d2.split_first_mut().unwrap();
    let alpha = *alpha;
    let beta = *beta;

    let compact_u = (u.nrows() != n + 1) as usize;

    if k <= jacobi_fallback_threshold || rem <= jacobi_fallback_threshold {
        let (mut u1_alloc, stack) =
            temp_mat_uninit::<E>(k + 1, compact_u * (k + 1), stack.rb_mut());
        let mut u1_alloc = u1_alloc.as_mut();
        let (mut u2_alloc, stack) = temp_mat_uninit::<E>(rem + 1, compact_u * (rem + 1), stack);
        let mut u2_alloc = u2_alloc.as_mut();

        let (_u0, mut u1, mut u2) = if compact_u == 0 {
            let (u1, u2) = u.rb_mut().split_at_row_mut(k + 1);
            let (u0, u1) = u1.split_at_col_mut(1);
            (
                u0,
                u1.submatrix_mut(0, 0, k + 1, k + 1),
                u2.submatrix_mut(0, k, rem + 1, rem + 1),
            )
        } else {
            (
                u.rb_mut().col_mut(0).as_2d_mut(),
                u1_alloc.rb_mut(),
                u2_alloc.rb_mut(),
            )
        };

        let (mut v1, mut v2) = match v.rb_mut() {
            Some(v) => {
                let (v1, v2) = v.split_at_row_mut(k);
                (
                    Some(v1.submatrix_mut(0, 1, k, k + 1)),
                    Some(v2.submatrix_mut(1, k, rem, rem + 1)),
                )
            }
            None => (None, None),
        };

        let (mut matrix1, stack) = temp_mat_zeroed::<E>(k + 1, k + 1, stack);
        let (mut matrix2, _) = temp_mat_zeroed::<E>(rem + 1, rem + 1, stack);
        let mut matrix1 = matrix1.as_mut();
        let mut matrix2 = matrix2.as_mut();

        for j in 0..k {
            matrix1.write(j, j, d1[j]);
            matrix1.write(j + 1, j, sub_d1[j]);
        }
        for j in 0..rem {
            matrix2.write(j, j + 1, d2[j]);
            matrix2.write(j + 1, j + 1, sub_d2[j]);
        }

        jacobi_svd(
            matrix1.rb_mut(),
            Some(u1.rb_mut()),
            v1.rb_mut(),
            Skip::Last,
            epsilon,
            consider_zero_threshold,
        );
        for j in 0..matrix1.ncols() {
            for i in 0..matrix1.nrows() {
                if i != j {
                    matrix1.write(i, j, E::faer_zero());
                }
            }
        }
        jacobi_svd(
            matrix2.rb_mut(),
            Some(u2.rb_mut()),
            v2.rb_mut(),
            Skip::First,
            epsilon,
            consider_zero_threshold,
        );
        for j in 0..matrix2.ncols() {
            for i in 0..matrix1.nrows() {
                if i != j {
                    matrix1.write(i, j, E::faer_zero());
                }
            }
        }

        if cfg!(debug_assertions) {
            if let Some(v1) = v1 {
                zipped!(v1.col_mut(k).as_2d_mut())
                    .for_each(|unzipped!(mut x)| x.write(E::faer_nan()));
            }
            if let Some(v2) = v2 {
                zipped!(v2.col_mut(0).as_2d_mut())
                    .for_each(|unzipped!(mut x)| x.write(E::faer_nan()));
            }
        }

        for j in 0..k {
            diag[j + 1] = matrix1.read(j, j);
        }
        for j in 0..rem {
            diag[j + k + 1] = matrix2.read(j + 1, j + 1);
        }

        if compact_u == 1 {
            // need to copy the first and last rows
            //
            // NOTE: we handle the rotation of (Q1, q1) here, so no need to handle it later when
            // compact_u == 1
            for (row, row1, row2) in [(0, 0, 0), (1, k, rem)] {
                zipped!(
                    u.rb_mut().row_mut(row).subcols_mut(1, k).as_2d_mut(),
                    u1_alloc.rb().row(row1).subcols(0, k).as_2d(),
                )
                .for_each(|unzipped!(mut dst, src)| dst.write(src.read()));
                u.write(row, 0, u1_alloc.read(row1, k));

                zipped!(
                    u.rb_mut().row_mut(row).subcols_mut(k + 1, rem).as_2d_mut(),
                    u2_alloc.rb().row(row2).subcols(1, rem).as_2d(),
                )
                .for_each(|unzipped!(mut dst, src)| dst.write(src.read()));
                u.write(row, n, u2_alloc.read(row2, 0));
            }
        } else {
            let (_, u2) = if compact_u == 0 {
                let (u1, u2) = u.rb_mut().split_at_row_mut(k + 1);
                (u1, u2)
            } else {
                let (u1, u2) = u.rb_mut().split_at_row_mut(1);
                (u1, u2)
            };

            let (left, right) = u2.split_at_col_mut(k + 1);
            let left = left.col_mut(k);
            let right = right.col_mut(rem);
            zipped!(right.as_2d_mut(), left.as_2d_mut()).for_each(
                |unzipped!(mut right, mut left)| {
                    right.write(left.read());

                    if cfg!(debug_assertions) {
                        left.write(E::faer_nan());
                    }
                },
            );
        }
    } else {
        let (mut u1, mut u2) = if compact_u == 0 {
            let (u1, u2) = u.rb_mut().split_at_row_mut(k + 1);
            (
                u1.submatrix_mut(0, 1, k + 1, k + 1),
                u2.submatrix_mut(0, k + 1, rem + 1, rem + 1),
            )
        } else {
            // NOTE: need to handle rotation of Q1, q1
            let (u1, u2) = u.rb_mut().split_at_col_mut(k + 1);
            (u1, u2)
        };

        let (mut v1, mut v2) = match v.rb_mut() {
            Some(v) => {
                let (v1, v2) = v.split_at_row_mut(k);
                (
                    Some(v1.submatrix_mut(0, 1, k, k)),
                    Some(v2.submatrix_mut(1, k + 1, rem, rem)),
                )
            }
            None => (None, None),
        };

        let stack_bytes = stack.len_bytes();
        let (mem1, stack2) = stack.rb_mut().make_raw::<u8>(stack_bytes / 2);
        let stack1 = PodStack::new(mem1);

        join_raw(
            |parallelism| {
                bidiag_svd_impl(
                    d1,
                    sub_d1,
                    u1.rb_mut(),
                    v1.rb_mut(),
                    true,
                    jacobi_fallback_threshold,
                    epsilon,
                    consider_zero_threshold,
                    parallelism,
                    stack1,
                );
            },
            |parallelism| {
                bidiag_svd_impl(
                    d2,
                    sub_d2,
                    u2.rb_mut(),
                    v2.rb_mut(),
                    true,
                    jacobi_fallback_threshold,
                    epsilon,
                    consider_zero_threshold,
                    parallelism,
                    stack2,
                );
            },
            parallelism,
        );

        if compact_u == 1 {
            // handle rotation of Q1, q1
            for i in (0..k).rev() {
                faer_core::permutation::swap_cols(u1.rb_mut(), i, i + 1);
            }
        }

        for i in (0..k).rev() {
            diag[i + 1] = diag[i];
        }
    }

    if let Some(mut v) = v.rb_mut() {
        v.write(k, 0, E::faer_one());
    };

    let lambda = if compact_u == 0 {
        u.read(k, k + 1)
    } else {
        // we already rotated u
        u.read(1, 0)
    };
    let phi = if compact_u == 0 {
        u.read(k + 1, n)
    } else {
        u.read(0, n)
    };

    let al = alpha.faer_mul(lambda);
    let bp = beta.faer_mul(phi);

    let r0 = ((al.faer_mul(al)).faer_add(bp.faer_mul(bp))).faer_sqrt();
    let (c0, s0) = if r0 == E::faer_zero() {
        (E::faer_one(), E::faer_zero())
    } else {
        (al.faer_div(r0), bp.faer_div(r0))
    };

    let col0 = subdiag;
    diag[0] = r0;
    col0[0] = r0;

    if compact_u == 0 {
        let (u1, u2) = if compact_u == 0 {
            let (u1, u2) = u.rb_mut().split_at_row_mut(k + 1);
            (u1, u2)
        } else {
            let (u1, u2) = u.rb_mut().split_at_row_mut(1);
            (u1, u2)
        };

        let (mut u0_top, u1) = u1.split_at_col_mut(1);
        let (u1, mut un_top) = u1.split_at_col_mut(n - 1);
        let (mut u0_bot, u2) = u2.split_at_col_mut(1);
        let (u2, mut un_bot) = u2.split_at_col_mut(n - 1);

        for j in 0..k {
            col0[j + 1] = alpha.faer_mul(u1.read(k, j));
        }
        for j in 0..rem {
            col0[j + 1 + k] = beta.faer_mul(u2.read(0, j + k));
        }

        zipped!(
            u0_top.rb_mut().col_mut(0).as_2d_mut(),
            un_top.rb_mut().col_mut(0).as_2d_mut(),
            u1.col_mut(k).as_2d_mut(),
        )
        .for_each(|unzipped!(mut dst0, mut dstn, mut src)| {
            let src_ = src.read();
            dst0.write(c0.faer_mul(src_));
            dstn.write(s0.faer_neg().faer_mul(src_));
            if cfg!(debug_assertions) {
                src.write(E::faer_nan());
            }
        });

        zipped!(
            u0_bot.rb_mut().col_mut(0).as_2d_mut(),
            un_bot.rb_mut().col_mut(0).as_2d_mut(),
        )
        .for_each(|unzipped!(mut dst0, mut dstn)| {
            let src_ = dstn.read();
            dst0.write(s0.faer_mul(src_));
            dstn.write(c0.faer_mul(src_));
        });
    } else {
        for j in 0..k {
            col0[j + 1] = alpha.faer_mul(u.read(1, j + 1));
            u.write(1, j + 1, E::faer_zero());
        }
        for j in 0..rem {
            col0[j + 1 + k] = beta.faer_mul(u.read(0, j + k + 1));
            u.write(0, j + k + 1, E::faer_zero());
        }

        let q10 = u.read(0, 0);
        let q21 = u.read(1, n);

        u.write(0, 0, c0.faer_mul(q10));
        u.write(0, n, s0.faer_neg().faer_mul(q10));
        u.write(1, 0, s0.faer_mul(q21));
        u.write(1, n, c0.faer_mul(q21));
    }

    let (perm, stack) = stack.rb_mut().make_with(n, |_| 0usize);
    let (jacobi_coeffs, stack) = stack.make_with(n, |_| JacobiRotation {
        c: E::faer_zero(),
        s: E::faer_zero(),
    });
    let (jacobi_indices, mut stack) = stack.make_with(n, |_| 0);

    let (jacobi_0i, jacobi_ij) = {
        let (transpositions, stack) = stack.rb_mut().make_with(n, |_| 0usize);
        deflate(
            diag,
            col0,
            jacobi_coeffs,
            jacobi_indices,
            u.rb_mut(),
            v.rb_mut(),
            transpositions,
            perm,
            k,
            epsilon,
            consider_zero_threshold,
            stack,
        )
    };

    let allocate_vm = v.is_some() as usize;
    let allocate_um = fill_u as usize;
    let (mut um, stack) = temp_mat_zeroed::<E>(n + 1, allocate_um * (n + 1), stack);
    let (mut vm, mut stack) = temp_mat_zeroed::<E>(n, allocate_vm * n, stack);
    let mut um = um.as_mut();
    let mut vm = vm.as_mut();

    compute_svd_of_m(
        fill_u.then_some(um.rb_mut()),
        v.is_some().then_some(vm.rb_mut()),
        diag,
        col0,
        perm,
        epsilon,
        consider_zero_threshold,
        stack.rb_mut(),
    );

    if fill_u {
        for (rot, &i) in jacobi_coeffs[..jacobi_0i]
            .iter()
            .zip(&jacobi_indices[..jacobi_0i])
            .rev()
        {
            let (um_top, um_bot) = um.rb_mut().split_at_row_mut(i);
            rot.apply_on_the_left_in_place(
                um_top.row_mut(0).as_2d_mut(),
                um_bot.row_mut(0).as_2d_mut(),
            );
        }
    }

    for (rot, &i) in jacobi_coeffs[jacobi_0i..][..jacobi_ij]
        .iter()
        .zip(&jacobi_indices[jacobi_0i..][..jacobi_ij])
        .rev()
    {
        let (i, j) = (i - 1, i);
        let mut actual_i = 0;
        let mut actual_j = 0;
        for (k, &p) in perm.iter().enumerate() {
            if p == i {
                actual_i = k;
            }
            if p == j {
                actual_j = k;
            }
        }

        if fill_u {
            let (row_i, row_j) = if actual_i < actual_j {
                let (um_top, um_bot) = um.rb_mut().split_at_row_mut(actual_j);
                (um_top.row_mut(actual_i), um_bot.row_mut(0))
            } else {
                let (um_top, um_bot) = um.rb_mut().split_at_row_mut(actual_i);
                (um_top.row_mut(actual_j), um_bot.row_mut(0))
            };
            rot.apply_on_the_left_in_place(row_i.as_2d_mut(), row_j.as_2d_mut());
        }

        if v.is_some() {
            let (row_i, row_j) = if actual_i < actual_j {
                let (vm_top, vm_bot) = vm.rb_mut().split_at_row_mut(actual_j);
                (vm_top.row_mut(actual_i), vm_bot.row_mut(0))
            } else {
                let (vm_top, vm_bot) = vm.rb_mut().split_at_row_mut(actual_i);
                (vm_top.row_mut(actual_j), vm_bot.row_mut(0))
            };
            rot.apply_on_the_left_in_place(row_i.as_2d_mut(), row_j.as_2d_mut());
        }
    }

    let _v_is_none = v.is_none();

    let mut update_v = |parallelism, stack: PodStack<'_>| {
        let (mut combined_v, _) = temp_mat_uninit::<E>(n, allocate_vm * n, stack);
        let mut combined_v = combined_v.as_mut();
        let v_rhs = vm.rb();

        if let Some(mut v) = v.rb_mut() {
            let mut combined_v = combined_v.rb_mut();
            let (mut combined_v1, combined_v2) = combined_v.rb_mut().split_at_row_mut(k);
            let mut combined_v2 = combined_v2.submatrix_mut(1, 0, rem, n);

            let v_lhs = v.rb();
            let v_lhs1 = v_lhs.submatrix(0, 1, k, k);
            let v_lhs2 = v_lhs.submatrix(k + 1, k + 1, rem, rem);
            let (v_rhs1, v_rhs2) = v_rhs.split_at_row(1).1.split_at_row(k);

            join_raw(
                |parallelism| {
                    faer_core::mul::matmul(
                        combined_v1.rb_mut(),
                        v_lhs1,
                        v_rhs1,
                        None,
                        E::faer_one(),
                        parallelism,
                    )
                },
                |parallelism| {
                    faer_core::mul::matmul(
                        combined_v2.rb_mut(),
                        v_lhs2,
                        v_rhs2,
                        None,
                        E::faer_one(),
                        parallelism,
                    )
                },
                parallelism,
            );

            faer_core::mul::matmul(
                combined_v.rb_mut().submatrix_mut(k, 0, 1, n),
                v_lhs.submatrix(k, 0, 1, 1),
                v_rhs.submatrix(0, 0, 1, n),
                None,
                E::faer_one(),
                parallelism,
            );

            zipped!(v.rb_mut(), combined_v.rb())
                .for_each(|unzipped!(mut dst, src)| dst.write(src.read()));
        }
    };

    let mut update_u = |parallelism, stack: PodStack<'_>| {
        let (mut combined_u, _) = temp_mat_uninit::<E>(n + 1, allocate_um * (n + 1), stack);
        let mut combined_u = combined_u.as_mut();

        if fill_u {
            let (mut combined_u1, mut combined_u2) = combined_u.rb_mut().split_at_row_mut(k + 1);
            let u_lhs = u.rb();
            let u_rhs = um.rb();
            let (u_lhs1, u_lhs2) = (
                u_lhs.submatrix(0, 0, k + 1, k + 1),
                u_lhs.submatrix(k + 1, k + 1, rem + 1, rem + 1),
            );
            let (u_rhs1, u_rhs2) = u_rhs.split_at_row(k + 1);

            join_raw(
                |parallelism| {
                    // matrix matrix
                    faer_core::mul::matmul(
                        combined_u1.rb_mut(),
                        u_lhs1,
                        u_rhs1,
                        None,
                        E::faer_one(),
                        parallelism,
                    );
                    // rank 1 update
                    faer_core::mul::matmul(
                        combined_u1.rb_mut(),
                        u_lhs.col(n).subrows(0, k + 1).as_2d(),
                        u_rhs2.row(rem).as_2d(),
                        Some(E::faer_one()),
                        E::faer_one(),
                        parallelism,
                    );
                },
                |parallelism| {
                    // matrix matrix
                    faer_core::mul::matmul(
                        combined_u2.rb_mut(),
                        u_lhs2,
                        u_rhs2,
                        None,
                        E::faer_one(),
                        parallelism,
                    );
                    // rank 1 update
                    faer_core::mul::matmul(
                        combined_u2.rb_mut(),
                        u_lhs.col(0).subrows(k + 1, rem + 1).as_2d(),
                        u_rhs1.row(0).as_2d(),
                        Some(E::faer_one()),
                        E::faer_one(),
                        parallelism,
                    );
                },
                parallelism,
            );

            zipped!(u.rb_mut(), combined_u.rb())
                .for_each(|unzipped!(mut dst, src)| dst.write(src.read()));
        }
    };

    if compact_u == 1 {
        update_v(parallelism, stack.rb_mut());
        if fill_u {
            let (mut combined_u, _) = temp_mat_uninit::<E>(2, n + 1, stack);
            let mut combined_u = combined_u.as_mut();
            faer_core::mul::matmul(
                combined_u.rb_mut(),
                u.rb(),
                um.rb(),
                None,
                E::faer_one(),
                parallelism,
            );
            zipped!(u.rb_mut(), combined_u.rb())
                .for_each(|unzipped!(mut dst, src)| dst.write(src.read()));
        }
    } else {
        match parallelism {
            #[cfg(feature = "rayon")]
            Parallelism::Rayon(_) if !_v_is_none => {
                let req_v = faer_core::temp_mat_req::<E>(n, n).unwrap();
                let (mem_v, stack_u) =
                    stack.make_aligned_raw::<u8>(req_v.size_bytes(), req_v.align_bytes());
                let stack_v = PodStack::new(mem_v);
                faer_core::join_raw(
                    |parallelism| update_v(parallelism, stack_v),
                    |parallelism| update_u(parallelism, stack_u),
                    parallelism,
                );
            }
            _ => {
                update_v(parallelism, stack.rb_mut());
                update_u(parallelism, stack);
            }
        }
    }

    for x in &mut *diag {
        *x = (*x).faer_mul(max_val);
    }
}

pub fn bidiag_real_svd_req<E: Entity>(
    n: usize,
    jacobi_fallback_threshold: usize,
    compute_u: bool,
    compute_v: bool,
    parallelism: Parallelism,
) -> Result<StackReq, SizeOverflow> {
    if n <= jacobi_fallback_threshold {
        temp_mat_req::<E>(n, n)
    } else {
        let _ = parallelism;
        let perm = StackReq::try_new::<usize>(n)?;
        let jacobi_coeffs = StackReq::try_new::<JacobiRotation<E>>(n)?;
        let jacobi_indices = perm;
        let transpositions = perm;
        let real_ind = perm;
        let real_col = perm;

        let um = temp_mat_req::<E>(n + 1, n + 1)?;
        let vm = temp_mat_req::<E>(n, if compute_v { n } else { 0 })?;

        let combined_u = temp_mat_req::<E>(if compute_u { n + 1 } else { 2 }, n + 1)?;
        let combined_v = vm;

        let prologue = StackReq::try_all_of([perm, jacobi_coeffs, jacobi_indices])?;

        StackReq::try_all_of([
            prologue,
            um,
            vm,
            combined_u,
            combined_v,
            transpositions,
            real_ind,
            real_col,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_approx_eq::assert_approx_eq;
    use faer_core::{assert, Mat};

    macro_rules! make_stack {
        ($req: expr) => {
            ::dyn_stack::PodStack::new(&mut ::dyn_stack::GlobalPodBuffer::new($req.unwrap()))
        };
    }

    // to avoid overflowing the stack
    macro_rules! vec_static {
    ($($x:expr),+ $(,)?) => (
        {
            static ARRAY: &[f64] = &[$($x),+];
            ARRAY.to_vec()
        }
    );
    }

    #[test]
    fn test_svd_n() {
        for n in [9, 16, 32, 64, 128, 256, 512, 1024] {
            let diag = (0..n).map(|_| rand::random::<f64>()).collect::<Vec<_>>();
            let subdiag = (0..n).map(|_| rand::random::<f64>()).collect::<Vec<_>>();
            dbg!(&diag, &subdiag);

            let n = diag.len();
            let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
            let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
            let s = {
                let mut diag = diag.clone();
                let mut subdiag = subdiag.clone();
                compute_bidiag_real_svd(
                    &mut diag,
                    &mut subdiag,
                    Some(u.as_mut()),
                    Some(v.as_mut()),
                    5,
                    0,
                    f64::EPSILON,
                    f64::MIN_POSITIVE,
                    Parallelism::None,
                    make_stack!(bidiag_real_svd_req::<f64>(
                        n,
                        5,
                        true,
                        true,
                        Parallelism::None
                    )),
                );
                Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
            };

            let reconstructed = &u * &s * v.transpose();
            for j in 0..n {
                for i in 0..n + 1 {
                    let target = if i == j {
                        diag[j]
                    } else if i == j + 1 {
                        subdiag[j]
                    } else {
                        0.0
                    };

                    assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
                }
            }
        }
    }

    #[test]
    fn test_svd_4() {
        let diag = vec_static![1.0, 2.0, 3.0, 4.0];
        let subdiag = vec_static![1.0, 1.0, 1.0];

        let n = diag.len();
        let mut u = Mat::from_fn(n, n, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            bidiag_svd_qr_algorithm_impl(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                f64::EPSILON,
                f64::MIN_POSITIVE,
            );
            Mat::from_fn(n, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }

    #[test]
    fn test_svd_64() {
        let diag = vec_static![
            0.5488135039273248,
            0.6027633760716439,
            0.4236547993389047,
            0.4375872112626925,
            0.9636627605010293,
            0.7917250380826646,
            0.5680445610939323,
            0.07103605819788694,
            0.02021839744032572,
            0.7781567509498505,
            0.978618342232764,
            0.46147936225293185,
            0.11827442586893322,
            0.1433532874090464,
            0.5218483217500717,
            0.26455561210462697,
            0.45615033221654855,
            0.018789800436355142,
            0.6120957227224214,
            0.9437480785146242,
            0.359507900573786,
            0.6976311959272649,
            0.6667667154456677,
            0.2103825610738409,
            0.31542835092418386,
            0.5701967704178796,
            0.9883738380592262,
            0.2088767560948347,
            0.6531083254653984,
            0.4663107728563063,
            0.15896958364551972,
            0.6563295894652734,
            0.1965823616800535,
            0.8209932298479351,
            0.8379449074988039,
            0.9764594650133958,
            0.9767610881903371,
            0.7392635793983017,
            0.2828069625764096,
            0.29614019752214493,
            0.317983179393976,
            0.06414749634878436,
            0.5666014542065752,
            0.5232480534666997,
            0.5759464955561793,
            0.31856895245132366,
            0.13179786240439217,
            0.2894060929472011,
            0.5865129348100832,
            0.8289400292173631,
            0.6778165367962301,
            0.7351940221225949,
            0.24875314351995803,
            0.592041931271839,
            0.2230816326406183,
            0.44712537861762736,
            0.6994792753175043,
            0.8137978197024772,
            0.8811031971111616,
            0.8817353618548528,
            0.7252542798196405,
            0.9560836347232239,
            0.4238550485581797,
            0.019193198309333526,
        ];
        let subdiag = vec_static![
            0.7151893663724195,
            0.5448831829968969,
            0.6458941130666561,
            0.8917730007820798,
            0.3834415188257777,
            0.5288949197529045,
            0.925596638292661,
            0.08712929970154071,
            0.832619845547938,
            0.8700121482468192,
            0.7991585642167236,
            0.7805291762864555,
            0.6399210213275238,
            0.9446689170495839,
            0.4146619399905236,
            0.7742336894342167,
            0.5684339488686485,
            0.6176354970758771,
            0.6169339968747569,
            0.6818202991034834,
            0.43703195379934145,
            0.06022547162926983,
            0.6706378696181594,
            0.1289262976548533,
            0.3637107709426226,
            0.43860151346232035,
            0.10204481074802807,
            0.16130951788499626,
            0.2532916025397821,
            0.24442559200160274,
            0.11037514116430513,
            0.1381829513486138,
            0.3687251706609641,
            0.09710127579306127,
            0.09609840789396307,
            0.4686512016477016,
            0.604845519745046,
            0.039187792254320675,
            0.1201965612131689,
            0.11872771895424405,
            0.41426299451466997,
            0.6924721193700198,
            0.2653894909394454,
            0.09394051075844168,
            0.9292961975762141,
            0.6674103799636817,
            0.7163272041185655,
            0.18319136200711683,
            0.020107546187493552,
            0.004695476192547066,
            0.27000797319216485,
            0.9621885451174382,
            0.5761573344178369,
            0.5722519057908734,
            0.952749011516985,
            0.8464086724711278,
            0.29743695085513366,
            0.39650574084698464,
            0.5812728726358587,
            0.6925315900777659,
            0.5013243819267023,
            0.6439901992296374,
            0.6063932141279244,
            0.30157481667454933,
        ];

        let n = diag.len();
        let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            compute_bidiag_real_svd(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                15,
                0,
                f64::EPSILON,
                f64::MIN_POSITIVE,
                Parallelism::None,
                make_stack!(bidiag_real_svd_req::<f64>(
                    n,
                    15,
                    true,
                    true,
                    Parallelism::None
                )),
            );
            Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n + 1 {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }

    #[test]
    fn test_svd_128() {
        let diag = vec_static![
            0.21604803719303378,
            0.3911685871373043,
            0.4717353802588816,
            0.5258511967180588,
            0.3495587375007383,
            0.29956620660149913,
            0.9617586737752963,
            0.1358382160501187,
            0.7927594027427639,
            0.7002703649721469,
            0.5011867621846828,
            0.5508360458872776,
            0.6671001529243077,
            0.6182640702775855,
            0.537113258218727,
            0.6494319004775305,
            0.22394544080467793,
            0.48963764985534675,
            0.4960750561790864,
            0.6762779313777806,
            0.4942507487962028,
            0.30598289084328734,
            0.7477750830615565,
            0.4134175601075717,
            0.16210479706508774,
            0.8554869501826141,
            0.9633922725373281,
            0.5178447186808554,
            0.4808128823427542,
            0.21235530384938095,
            0.34390969950363515,
            0.5222397627933266,
            0.8078540262388403,
            0.3084527067162488,
            0.8510243010197533,
            0.7492658574080864,
            0.2971760706318315,
            0.5821109217348188,
            0.9355688927263782,
            0.6568884170143395,
            0.7143623902994362,
            0.8745547764908594,
            0.3166725157072694,
            0.06280104609776738,
            0.9988219571244557,
            0.3034566490500038,
            0.6043605519679998,
            0.5327046414132618,
            0.8160784550544813,
            0.33220426954591,
            0.3160884461616036,
            0.08177180318496124,
            0.5859174456552851,
            0.225028943377522,
            0.6862486995947498,
            0.3697307197174694,
            0.7873879339970076,
            0.21908989285489933,
            0.5410943047067103,
            0.6302243946164361,
            0.17747192668740164,
            0.6281781273604742,
            0.5854835895783808,
            0.6512696242357562,
            0.559113383282545,
            0.7596325146050337,
            0.09312745837133651,
            0.501703867727036,
            0.949275885265856,
            0.620974047588036,
            0.3874150582755552,
            0.7083379430913563,
            0.75288059477905,
            0.1270527976228708,
            0.23126586686009443,
            0.12024520920717885,
            0.5679798123160427,
            0.17978590193238875,
            0.6968486822029739,
            0.7157516389948776,
            0.863508815070862,
            0.15864367506453403,
            0.11417600568460162,
            0.9651132813595767,
            0.10920826282790252,
            0.28700997153205676,
            0.7054115856120382,
            0.3490250121285702,
            0.29128696537701393,
            0.9304161241740285,
            0.2268455711369768,
            0.7658439715371834,
            0.06071820836196018,
            0.027168754664580574,
            0.4433866338712438,
            0.8175541779071445,
            0.1195115235309906,
            0.5543104624561522,
            0.3831276253977298,
            0.4944969243226346,
            0.5069163526882893,
            0.2519761931522101,
            0.3802289988930322,
            0.12792877754948118,
            0.964418293878996,
            0.5028833771104924,
            0.7140891912929843,
            0.929920514299548,
            0.9622470160446642,
            0.9165762824392009,
            0.957409262046926,
            0.046890401426795014,
            0.9559558333706967,
            0.10165169845100896,
            0.4030729711821963,
            0.7457966905965205,
            0.45506350389528505,
            0.22855385350793034,
            0.5774367409801651,
            0.3395031602763888,
            0.8750661230154188,
            0.5436696130661226,
            0.14750222902451415,
            0.4702601766248026,
            0.380398914581512,
            0.9870908933390458,
            0.46972043263478913,
            0.7629347676594994,
        ];
        let subdiag = vec_static![
            0.5656024571426115,
            0.45845513013153916,
            0.8464475246274293,
            0.5477997098985157,
            0.8505749005789477,
            0.036655381821360744,
            0.2164923701172936,
            0.992162216073592,
            0.2442305366823595,
            0.5417621610202344,
            0.4608606260638025,
            0.41227354070140787,
            0.9159226592102876,
            0.41719392867697913,
            0.30450224568165174,
            0.913124938919393,
            0.8998452924705163,
            0.8615757311218621,
            0.2152284123127688,
            0.4290329466601026,
            0.5684735244446122,
            0.5679483074313831,
            0.34457343811812624,
            0.5415296298206568,
            0.8356244784272918,
            0.8166459785498866,
            0.5080772280859633,
            0.8956149463267449,
            0.04644596806209644,
            0.3433783039423306,
            0.6431291583487241,
            0.4639223708461163,
            0.8505923626098933,
            0.04989146706988801,
            0.7831842489006767,
            0.27932627070175853,
            0.4802138742827945,
            0.5760948972572593,
            0.5545322682351106,
            0.21873418294084268,
            0.431998289203515,
            0.28960427805813527,
            0.20805283252719742,
            0.4598338664038314,
            0.03434413796223135,
            0.8739945690639832,
            0.5815729275918338,
            0.19359623734135956,
            0.7141431528881316,
            0.5483053379185088,
            0.3859904909506836,
            0.32779561877996766,
            0.7473245156367854,
            0.7527840401770349,
            0.5332180778014928,
            0.18845765841689788,
            0.8863719798459543,
            0.04292531786719711,
            0.7455031922487241,
            0.46581152947672433,
            0.2528295777598255,
            0.3175198547141883,
            0.45895268958636226,
            0.5910400449400208,
            0.5907671590866751,
            0.7200711220634236,
            0.06754760808821003,
            0.4622760636221317,
            0.6725272498965258,
            0.842299904498038,
            0.675399181783893,
            0.8815937503757029,
            0.4030870020741989,
            0.7417783045865814,
            0.27829985877736574,
            0.16223373545195352,
            0.047472863984402536,
            0.9762183713220033,
            0.850015796049705,
            0.9883602721062648,
            0.7183825826458256,
            0.6776325302074241,
            0.5606755750903751,
            0.4901171002893656,
            0.726732063504903,
            0.5776852831317164,
            0.7123622484466453,
            0.755897963752805,
            0.5446524557651221,
            0.3337687893180128,
            0.3075795178050936,
            0.8001257236535856,
            0.18895675106770227,
            0.3579844168353461,
            0.3527072586559228,
            0.0817878204567436,
            0.9969511917959174,
            0.6404417022308447,
            0.28418361844714657,
            0.6511969684943811,
            0.2866537500197578,
            0.33561205627307966,
            0.2534861786545628,
            0.9960188356208826,
            0.10107370019257966,
            0.6295541630401397,
            0.5638169140807354,
            0.7619672332144566,
            0.19599633482758116,
            0.004262801281641138,
            0.5637409510062904,
            0.15132931034408448,
            0.6357856412777408,
            0.3570943637285525,
            0.8986379725558856,
            0.6123637576833882,
            0.21915528413252194,
            0.35614983876651884,
            0.9830902701738703,
            0.2656035472280712,
            0.8894933827255144,
            0.5619386899890909,
            0.019131462959508294,
            0.36198104450872315,
            0.8387140376274692,
            0.2967905230261907,
            0.009493153610368066,
            0.4451542437520205,
        ];

        let n = diag.len();
        let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            compute_bidiag_real_svd(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                40,
                0,
                f64::EPSILON,
                f64::MIN_POSITIVE,
                Parallelism::None,
                make_stack!(bidiag_real_svd_req::<f64>(
                    n,
                    40,
                    true,
                    true,
                    Parallelism::None
                )),
            );
            Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n + 1 {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }

    #[test]
    fn test_svd_1024_0() {
        let diag = vec_static![
            0.8845251118613418,
            0.34455256888844044,
            0.210674711024898,
            0.19415197496797754,
            0.9667549946932977,
            0.9929941756848952,
            0.3449032796124365,
            0.013043707957282269,
            0.5379750826661898,
            0.9878349516485647,
            0.7840176804531493,
            0.05421416657132472,
            0.6276152195489152,
            0.5302903207188766,
            0.1489678571817551,
            0.5910099656870687,
            0.8125771508983507,
            0.019854461222473585,
            0.23300742519619422,
            0.9261309512615169,
            0.5099296602111684,
            0.396690210400782,
            0.2657945708665065,
            0.04886273313636724,
            0.5138205614876258,
            0.12184534545958348,
            0.8914471736391029,
            0.9265260573331591,
            0.4878431401362272,
            0.7697237381965547,
            0.009936509018894535,
            0.8800411633128924,
            0.41970310146014045,
            0.053141783595033076,
            0.6082362328591278,
            0.4297917877465598,
            0.9264338860742358,
            0.20337132094924437,
            0.6186986895951906,
            0.514494342538388,
            0.36647591921360345,
            0.7909607320635065,
            0.11648430104115892,
            0.9981247894173411,
            0.4214625077906733,
            0.27873435601711005,
            0.06275412441803119,
            0.17994040410520007,
            0.5767826619151486,
            0.8276287407761077,
            0.4856049420452119,
            0.1824024117042553,
            0.380547967315335,
            0.18277861527693784,
            0.8560272319230807,
            0.7352358350367258,
            0.37244824553668243,
            0.08858898352613309,
            0.15670394303205137,
            0.9549608259831481,
            0.4609226155156112,
            0.2752940722916162,
            0.14648689328571252,
            0.24702880747653244,
            0.26413558185873487,
            0.25963973131499496,
            0.7874760238719776,
            0.5392390398518286,
            0.43108494111567286,
            0.9602150966834309,
            0.263919286112432,
            0.5519733682457418,
            0.27087432517628174,
            0.41373002164894046,
            0.4961242345741296,
            0.8786873455034356,
            0.22141198367945658,
            0.12815828684009156,
            0.24793784677162356,
            0.3242928455356374,
            0.5232575470210502,
            0.781790771620908,
            0.2378803144741315,
            0.9224957075362035,
            0.8654377128841579,
            0.22767383234003058,
            0.6367731312601166,
            0.8505361623040123,
            0.020146188482329075,
            0.5206013694815037,
            0.36776570341882464,
            0.37256832562995335,
            0.2371097865141898,
            0.32354779308058046,
            0.3840014267954045,
            0.9273526050829554,
            0.6974410601518757,
            0.4684376632291788,
            0.33199244231586855,
            0.1592208015718991,
            0.8457916545164874,
            0.7594540340761741,
            0.5977160139755071,
            0.2448529282667069,
            0.7422321131631072,
            0.4491255167076257,
            0.877793952642107,
            0.2061115436687152,
            0.5955960979297951,
            0.3641242521958884,
            0.06830447764964187,
            0.29548562339469486,
            0.14656535751600874,
            0.9347902426607325,
            0.551454375693908,
            0.05755666167494633,
            0.1182662384866503,
            0.9969493917667304,
            0.7774163485337313,
            0.7232409678464244,
            0.953755416217937,
            0.13987247560464577,
            0.8056655147304023,
            0.8381653208805445,
            0.5208871733781028,
            0.7353011028495107,
            0.7522163928333057,
            0.10541375581323387,
            0.09041155597045147,
            0.8667408478176504,
            0.9654448739439632,
            0.005792304462705622,
            0.2335819251562139,
            0.5369003975588765,
            0.03737234922010857,
            0.4588058730077743,
            0.884749406352933,
            0.07830951962815003,
            0.05162426233203987,
            0.1968400879118768,
            0.8007876035442365,
            0.7968477315552086,
            0.8047714077241233,
            0.2765763666146831,
            0.19054608072462764,
            0.5407650387375886,
            0.44575925860601684,
            0.69988681364929,
            0.02921195559473666,
            0.9519063242299393,
            0.2292637344597196,
            0.57168345100491,
            0.6119802248741711,
            0.9061002127200055,
            0.835234531347637,
            0.21775743544975845,
            0.598015663069716,
            0.7246168958089019,
            0.18660869930659219,
            0.6860807800890482,
            0.6207537425828924,
            0.04036114082282971,
            0.2596034256687044,
            0.9263145409044506,
            0.12006520113520502,
            0.06526114830309226,
            0.6060749180627996,
            0.830726692759335,
            0.8819438566592785,
            0.2823300181080588,
            0.004762366303322829,
            0.15705700354656182,
            0.872273157439614,
            0.05161458570595012,
            0.19404590278273093,
            0.673337106135467,
            0.5276143405427874,
            0.15032516518663774,
            0.7269693382522674,
            0.6496354105288265,
            0.8783772718768026,
            0.7230285777369317,
            0.8006872911221266,
            0.6166519065388856,
            0.9526515838852074,
            0.13932837641394247,
            0.27769520707366524,
            0.6915062763055476,
            0.512026353250563,
            0.2632486782448227,
            0.5995502982365921,
            0.8976384724135182,
            0.43965952491907645,
            0.05856887773794872,
            0.04886342450006653,
            0.46804720854588455,
            0.7525286155087892,
            0.9556104870431635,
            0.9135617349595734,
            0.10507903894086212,
            0.3874922350178007,
            0.9433755059296061,
            0.40312789184461495,
            0.7281420809216822,
            0.7473556564126743,
            0.13580390254853258,
            0.4793108553831614,
            0.6077475752583249,
            0.09916537750409427,
            0.984284070196559,
            0.8563424060832624,
            0.5371224391731257,
            0.6848273225152729,
            0.6507685185025187,
            0.5547937322274868,
            0.4056063327295283,
            0.5804860496295368,
            0.8124064239085033,
            0.1602324786734518,
            0.09880451576259175,
            0.09935758772113779,
            0.8971081077362497,
            0.11222279366053156,
            0.42060122955982093,
            0.6114566179885966,
            0.6453598215339088,
            0.4912286655584044,
            0.8837577596587839,
            0.601795323666988,
            0.7152818307776255,
            0.5926042612262687,
            0.36686793218606273,
            0.0313842872598018,
            0.36981194752406976,
            0.007013645381377498,
            0.6233497518351521,
            0.19247812929961905,
            0.3366789389253526,
            0.4837733261985061,
            0.7060286649954945,
            0.6560485260353782,
            0.30678422327474575,
            0.05424227380794244,
            0.9809566991181687,
            0.9679876980114167,
            0.37060417403309087,
            0.8876909232855882,
            0.9266175828719014,
            0.6157004519300252,
            0.4742621496185583,
            0.4716225437585564,
            0.42558581920979377,
            0.6870915543189346,
            0.2106157507697909,
            0.7148685882645731,
            0.29741609982987893,
            0.05585922166871293,
            0.16842926321869744,
            0.269368257543357,
            0.09856436162547,
            0.6318795405201129,
            0.06554775449804462,
            0.7513407348457041,
            0.2836215405898581,
            0.1328047454725776,
            0.09368346629167423,
            0.5376447868406243,
            0.3562742880762211,
            0.6976956411522769,
            0.8652060945219386,
            0.6927438310591169,
            0.8055640024374024,
            0.7116858599057524,
            0.6436185630532363,
            0.00622643683652091,
            0.45442170067986876,
            0.1372780376144771,
            0.9439243200885419,
            0.5291647990839722,
            0.5027492197791616,
            0.9617660041030734,
            0.7804120429985603,
            0.27125510499764616,
            0.8643033678059027,
            0.5619503443692003,
            0.8188456709963385,
            0.4012996542042663,
            0.23310987710285913,
            0.8899985323819251,
            0.4664831476280643,
            0.6729125114401061,
            0.9664025202400076,
            0.43486799439819446,
            0.8974277439810396,
            0.4808449065701321,
            0.11506094057610716,
            0.25379791153831577,
            0.03964053497879083,
            0.36909125512271623,
            0.4029973377655117,
            0.009001224984161449,
            0.9758190049357144,
            0.39023786547431394,
            0.9112427690611551,
            0.779990667772811,
            0.9036018460133876,
            0.18512984695811086,
            0.9178143533632862,
            0.4278781144480831,
            0.661581165339597,
            0.6343292826215128,
            0.31166415713361884,
            0.856217051781153,
            0.3196697246058282,
            0.1245222224484781,
            0.7728908759067107,
            0.9636719737186232,
            0.9340817362172681,
            0.3221972107257043,
            0.31519271142473704,
            0.8976309840829237,
            0.7086399432314598,
            0.04136495166094478,
            0.07658868727707802,
            0.2363447085818272,
            0.8878358508273737,
            0.9174891002547204,
            0.781061898467373,
            0.5988437757616014,
            0.29070843935762014,
            0.44093957477344725,
            0.11435842604864599,
            0.38771810414159247,
            0.009518348153462197,
            0.06806110689805611,
            0.62129101334924,
            0.11035187903178978,
            0.45351609034461293,
            0.47733641219479117,
            0.059036119775712326,
            0.28778565060683126,
            0.7395520480546994,
            0.6364194028524534,
            0.33159486012856376,
            0.13146467242925763,
            0.20926575331965835,
            0.5914783076243179,
            0.14538250921489848,
            0.3285367514346902,
            0.14111259631818374,
            0.9271206639868662,
            0.4791305932719009,
            0.42484371918265673,
            0.9243113101525169,
            0.04657014131556869,
            0.47012270169625714,
            0.017578423681492317,
            0.41192041951803826,
            0.5435010948082887,
            0.830019400684995,
            0.9838262050532701,
            0.31155683385462063,
            0.3395810989977315,
            0.008514249063874657,
            0.36804963249184464,
            0.39065717416407375,
            0.48060664877288317,
            0.9177524734572041,
            0.9963808703554067,
            0.46762091468546574,
            0.2190669248616377,
            0.7546402963954308,
            0.3826675586492012,
            0.4519670427156247,
            0.10034147317999753,
            0.9339045232941123,
            0.6861661261352751,
            0.01108362938610885,
            0.42186561885257856,
            0.6049633961733282,
            0.6665693539826331,
            0.43832298084278354,
            0.28759644404552964,
            0.42589599699235514,
            0.6215685157259329,
            0.5819897901940292,
            0.654993364802773,
            0.8849572516939255,
            0.056661097249976033,
            0.6252616876482108,
            0.12912956119956975,
            0.5937043009630114,
            0.3567979519308234,
            0.6651293721946168,
            0.4162450059551198,
            0.7866799944868544,
            0.33559766465195273,
            0.16910220850833024,
            0.2557155444695407,
            0.1371622094265289,
            0.5169211909209518,
            0.27000552870068484,
            0.8070007223783799,
            0.35499549142147346,
            0.014302822401554782,
            0.22722151291096326,
            0.07998829912888339,
            0.8194348476014353,
            0.88446073640522,
            0.6619719078328444,
            0.9367053935121534,
            0.23570771605208773,
            0.844467854244812,
            0.08641849711080207,
            0.1608054804669894,
            0.1752041524603587,
            0.6092665138366469,
            0.33373985941726614,
            0.9113357896883022,
            0.11595639850509243,
            0.7068757086199514,
            0.7865607768581245,
            0.9346872146042198,
            0.05564406010499112,
            0.2699085433214231,
            0.05591730860133559,
            0.6608433401410738,
            0.530444670200964,
            0.8319757527334114,
            0.486955173829685,
            0.41444998950490164,
            0.7846459652439971,
            0.07682909713484198,
            0.028896143688622145,
            0.7735911432210456,
            0.8561347987430765,
            0.6833766669122127,
            0.732403990897978,
            0.07393737509067311,
            0.6859173206119581,
            0.10353213742451339,
            0.9490710297122695,
            0.34603734460546876,
            0.7042465786381801,
            0.6744746690394707,
            0.734932043801607,
            0.41656957539578376,
            0.5227623335281744,
            0.32679798434601914,
            0.713091728672727,
            0.10090676539403032,
            0.08333177896943456,
            0.43136813553231124,
            0.5553735253983637,
            0.9938059660537949,
            0.2038100855893512,
            0.7904924245747698,
            0.7654582607613682,
            0.2632496613842946,
            0.5032987983538753,
            0.10059506853686151,
            0.05392849355937801,
            0.27997229159761305,
            0.05738118418904803,
            0.5888198410031323,
            0.5496228609661271,
            0.18124563158960938,
            0.9279548341660727,
            0.22356900241713673,
            0.5074439639227654,
            0.8465638446831127,
            0.9939222841611288,
            0.009203738044384568,
            0.9035625463960713,
            0.5478498401870193,
            0.1436740504780365,
            0.11030013507966019,
            0.49435562218779827,
            0.36474996817626115,
            0.8690729143610514,
            0.27512438505007153,
            0.05519547164147387,
            0.19233743278188775,
            0.0664409503721326,
            0.6489080576699797,
            0.019063712733404903,
            0.1534430886662448,
            0.24480545379252816,
            0.01051499963138458,
            0.6856116940528466,
            0.7773097044715064,
            0.3677858272104545,
            0.5607469272924686,
            0.08438163910055485,
            0.4927293124318882,
            0.3758536484533026,
            0.9852448384568894,
            0.38038214870756404,
            0.4768862738938858,
            0.42504128314102363,
            0.884574088023932,
            0.7447948103427381,
            0.6281952560439542,
            0.8422943316973285,
            0.11117974951930387,
            0.6788820387560264,
            0.9748171029633533,
            0.3112345837082654,
            0.19692145192748078,
            0.1898685764540009,
            0.9232274745660699,
            0.17044419712823644,
            0.4851270278696377,
            0.5536690025965553,
            0.209418220241673,
            0.6259250634518518,
            0.7543334303125396,
            0.2937849309999855,
            0.3729476658139417,
            0.39313457462054435,
            0.06974764236154585,
            0.10344310048189886,
            0.507518560753304,
            0.43774470454608494,
            0.8364146679204518,
            0.9070492787267092,
            0.9567774773222549,
            0.10500970235931761,
            0.3998498929261717,
            0.29717649355166853,
            0.7615834186247338,
            0.6342900101100667,
            0.2849863073120511,
            0.037515223183286706,
            0.1435650512414116,
            0.4946558413658533,
            0.24838964588562307,
            0.06953183293100962,
            0.010979908802799532,
            0.6470620854580164,
            0.1489505156426364,
            0.9318992263165846,
            0.11352771228732728,
            0.16700430653862086,
            0.4766929353339845,
            0.5097455503317575,
            0.4326982921969347,
            0.7741042678051568,
            0.08165168787991572,
            0.5578748265687337,
            0.2308499062220727,
            0.5123779157582458,
            0.24100763785021273,
            0.10886023061825767,
            0.3463078209397997,
            0.4346935062407594,
            0.3541824862033963,
            0.8506208375314257,
            0.11865040517795522,
            0.8787835948303524,
            0.7514204500151836,
            0.3537273898842549,
            0.18495105215340069,
            0.8794538168214154,
            0.43846955371546736,
            0.6223808367304243,
            0.2532422174248372,
            0.0173945729559013,
            0.825120152220085,
            0.8343521374210742,
            0.24514667775364773,
            0.5864829268587433,
            0.06114537616802951,
            0.08947759755430962,
            0.2670467129935481,
            0.6218659288801367,
            0.5701826218390645,
            0.30123481898013516,
            0.3555024041627374,
            0.5942989396022218,
            0.5850733554368567,
            0.3977819861337537,
            0.26570347091195734,
            0.25740366564861905,
            0.46423759145504806,
            0.40930616359772365,
            0.8642930629057376,
            0.8548459173798607,
            0.5610185499351202,
            0.9079259267908577,
            0.9540217218606191,
            0.9877136190157554,
            0.25638467812786225,
            0.45228748162692256,
            0.23021853631358136,
            0.7606298373681132,
            0.11809446396524093,
            0.5464203016842142,
            0.82014799173527,
            0.5673485070159224,
            0.28189744630354674,
            0.5728473340310175,
            0.12745779045010586,
            0.2093600651192693,
            0.040409181249943193,
            0.5437097498036081,
            0.4698843713650859,
            0.3744100758683092,
            0.8820853651881632,
            0.09661682517079428,
            0.44008282207016947,
            0.7661341654608439,
            0.5002899778280783,
            0.939935188435343,
            0.4037845646767523,
            0.4754687371351335,
            0.12348699298976351,
            0.8328535547922692,
            0.7550678974094668,
            0.13420599429716162,
            0.37226957043440323,
            0.35897133577902696,
            0.2839857243007452,
            0.30151359234377895,
            0.4873323691626037,
            0.09644463427460526,
            0.3068651283752245,
            0.19457042965184612,
            0.13193683664769307,
            0.15402117137314475,
            0.12060810096037711,
            0.47588374471922557,
            0.9825405359602971,
            0.09506011601995101,
            0.0951473299180351,
            0.17552464191224793,
            0.5446585979359402,
            0.3933775183583844,
            0.5313822262288094,
            0.638956815956248,
            0.9173221237559014,
            0.4995138447644756,
            0.2610913790829166,
            0.4833107536687732,
            0.9971758070471496,
            0.3421854083206227,
            0.5080486727041559,
            0.4256910136975586,
            0.32446998261012305,
            0.08767191717339928,
            0.4374180631006207,
            0.26125518794943137,
            0.44389514759815984,
            0.9388266039769766,
            0.8201374509465024,
            0.7438169768604541,
            0.8032628491632073,
            0.09098688736942062,
            0.8547640562601011,
            0.5552333141514629,
            0.36260676800032277,
            0.5034045469637766,
            0.15186149958437956,
            0.3944636144675695,
            0.3721326486252522,
            0.923337028171655,
            0.41430211287983776,
            0.2602515627476548,
            0.9397086532157933,
            0.5230207413511277,
            0.475892714054884,
            0.694683555642716,
            0.5118255773056839,
            0.3113994477099723,
            0.5547390653920204,
            0.5893906407999145,
            0.7659093103586729,
            0.5075122380840992,
            0.18283934901868948,
            0.5698607238462075,
            0.13449423615296285,
            0.045459918245799424,
            0.08625964304367006,
            0.417395859295686,
            0.261062158102505,
            0.8075298740340944,
            0.8883665580333145,
            0.4889019655098171,
            0.2221188930219562,
            0.2816496152946476,
            0.15494833191304713,
            0.21418617334287815,
            0.3237557989949549,
            0.9196198742987929,
            0.18572851649416433,
            0.5285212603645132,
            0.9355753348529846,
            0.6891075139555204,
            0.257586325634688,
            0.43689292209889663,
            0.8657427762269848,
            0.20275374629883136,
            0.928494262842214,
            0.48760667502157895,
            0.40064262508174053,
            0.2925932318415707,
            0.2143069075286741,
            0.40430984322007,
            0.4463752172022737,
            0.9123971917156188,
            0.7471055164332462,
            0.12019130373697817,
            0.6346911798610109,
            0.404160753722707,
            0.38544851846509864,
            0.7365095934005528,
            0.6581005716429144,
            0.27835749174429714,
            0.8608667648331466,
            0.10716728595973024,
            0.7642064541269226,
            0.7732208264521794,
            0.24547528427024534,
            0.8093161349811911,
            0.281236029404172,
            0.5399086933128905,
            0.9555409254750834,
            0.8728156769659005,
            0.2392265872077518,
            0.41583479392446654,
            0.26231913777793525,
            0.882082615484338,
            0.7285497107647825,
            0.5363502438082618,
            0.21407252245857544,
            0.9790657697354228,
            0.34832348843681604,
            0.8995015876461342,
            0.5399994071733517,
            0.627367538005929,
            0.8936327303225186,
            0.1458309386700306,
            0.39889901760514235,
            0.021967589354523587,
            0.27632921178999303,
            0.44994401565091635,
            0.4526171030273327,
            0.7152832299491756,
            0.5370716005441079,
            0.9290413356376639,
            0.7638540725626427,
            0.29043023254730993,
            0.028544561435507876,
            0.2597221997555347,
            0.028028354812694056,
            0.016281249462922864,
            0.920307786946952,
            0.10851409325535322,
            0.37557097156207275,
            0.7966291035142514,
            0.2150703311155887,
            0.12780106678926428,
            0.017508933064456667,
            0.8615446835117896,
            0.5377076451210898,
            0.8495505289919821,
            0.45096310987004407,
            0.5250602002063562,
            0.07124839465430677,
            0.6185298746083393,
            0.2814465744642637,
            0.8171841827449374,
            0.7584862446805867,
            0.12567788435821203,
            0.5930540147639154,
            0.8849415301031589,
            0.17427330180330625,
            0.2010378264968402,
            0.39174436702226,
            0.9012408312896981,
            0.1386741748649597,
            0.38639748547112107,
            0.09006322004054756,
            0.9082962363057114,
            0.811784624155095,
            0.6743784350882154,
            0.09906487768907013,
            0.9309469870173297,
            0.8897197263370551,
            0.12541115209796438,
            0.23312512510466665,
            0.5132385782782324,
            0.7275904539321562,
            0.9057213452366281,
            0.7297530279941175,
            0.791978695353606,
            0.37092185205852835,
            0.8483398896367347,
            0.30955294767063535,
            0.771189644766232,
            0.2906290667202338,
            0.7453198057550258,
            0.21653899571012458,
            0.9775513518255299,
            0.7254220951183088,
            0.8668113703090181,
            0.6194065009415832,
            0.2863451172116679,
            0.5667190680021205,
            0.925653625989403,
            0.033560039914012574,
            0.8973876639754387,
            0.7224153371687022,
            0.8064677351942471,
            0.5780794168031764,
            0.7660235251315161,
            0.2620511545407739,
            0.5841291237263148,
            0.40459159694220515,
            0.12814289255326505,
            0.4588594921757614,
            0.20444429900397298,
            0.943652543350375,
            0.16833992985205037,
            0.7340254790523432,
            0.941407765783495,
            0.8125595396371769,
            0.5615635814574278,
            0.023829005699015915,
            0.0894218297915016,
            0.9726621750060844,
            0.27371882433441985,
            0.820099804114962,
            0.6495996966118213,
            0.16101762786768825,
            0.1423893361655213,
            0.5309658651157899,
            0.7697814537672145,
            0.7580719157068739,
            0.9399829889011113,
            0.3639016293263453,
            0.8838453513063597,
            0.39192310465767566,
            0.8617173246541459,
            0.805017619875162,
            0.6524173955629243,
            0.4596275958450332,
            0.7026014717755514,
            0.15674099767732896,
            0.9306233380687109,
            0.03954219378811974,
            0.5429358106690747,
            0.9574396141996595,
            0.3423698971057674,
            0.2447146653935679,
            0.09921039521270503,
            0.7378442271637177,
            0.12563985902014574,
            0.5326702755764166,
            0.045217673715322926,
            0.38389292763866045,
            0.5354917794116903,
            0.5845898831760369,
            0.16812202883973026,
            0.5485428102270978,
            0.8416052158014439,
            0.6206682880766721,
            0.4435313057311998,
            0.151713060737244,
            0.48157407311325273,
            0.6986899281158176,
            0.2210887015121874,
            0.07005386341944864,
            0.014868415754305087,
            0.5194135227188144,
            0.39088624580450126,
            0.7605832049265179,
            0.6762673208469522,
            0.8291607514571048,
            0.5767977185364629,
            0.1476360889153545,
            0.8713989228781874,
            0.4201968970228106,
            0.27597036603247105,
            0.20622555712119583,
            0.23377214116657952,
            0.6378667544453603,
            0.4808971835768795,
            0.6942176290429053,
            0.7514562431659588,
            0.3254218376076441,
            0.7759065993020333,
            0.8688364664055,
            0.058653408092502635,
            0.49549567998888433,
            0.03657833451630399,
            0.16917910139727643,
            0.42411804891524285,
            0.9161646353617822,
            0.5736335260984191,
            0.8560891920908282,
            0.8295971825773533,
            0.6198112877891949,
            0.9533525740629326,
            0.2228519083099909,
            0.12958204310042476,
            0.02964863024908193,
            0.5827534617648027,
            0.4744264395380964,
            0.29056315277638267,
            0.6576898201570917,
            0.942398882416274,
            0.6366537862128913,
            0.7155637078822991,
            0.3371336756466209,
            0.9127030992249006,
            0.6100868652627647,
            0.5419675813942905,
            0.4314235263828414,
            0.2229787266376665,
            0.5751203571398641,
            0.5377920941894427,
            0.6184651925697617,
            0.10166459142369644,
            0.5006884922345262,
            0.8473281521128757,
            0.354027469370917,
            0.989332514582519,
            0.6564861907905184,
            0.571402636513833,
            0.6317054209816227,
            0.3694852621662059,
            0.7574513830542177,
            0.45679332668735106,
            0.1271953777164424,
            0.5910815545784368,
            0.813904952296986,
            0.02310514525211138,
            0.7424699129535078,
            0.3915098149952104,
            0.01155938751400587,
            0.4031433809049668,
            0.678027647415598,
            0.7082527232743558,
            0.9353783729655919,
            0.8801376399629393,
            0.5387820724455229,
            0.2722569005205504,
            0.1470331398956941,
            0.8260710776157071,
            0.713566911335019,
            0.9170469271968411,
            0.13936763708492894,
            0.42161327076184363,
            0.28424349125683035,
            0.3671033730334905,
            0.13246773458329553,
            0.8973331016642114,
            0.6665503100026733,
            0.757804611455464,
            0.5896758029668179,
            0.19844119526434956,
            0.07843991721429922,
            0.9603382105509987,
            0.6974678498027387,
            0.9912625862864402,
            0.781246847308964,
            0.07999498724724818,
            0.8913156249997439,
            0.3145980902194806,
            0.12647260044256647,
            0.7946548793044863,
            0.033871301015447,
            0.5504501814363837,
            0.4511052584536204,
            0.774098664423315,
            0.2487470001472769,
            0.6067861889279945,
            0.6685445735264952,
            0.38709013510050394,
            0.9169539240621843,
            0.9500984625411802,
            0.8067182843492807,
            0.03180693064936568,
            0.6709304220148694,
            0.18076652905884238,
            0.250368676753443,
            0.2860975092729513,
            0.8710391195602349,
            0.167497840100998,
            0.7353235414942768,
            0.9352292778787517,
            0.4511306842770142,
            0.3827758461191245,
            0.281066928172492,
            0.6980469279053299,
            0.9201458951964375,
            0.016736962594040006,
            0.18537591560445787,
            0.6421454251873325,
            0.14953390974062974,
            0.3545167086497891,
            0.8357720236520648,
            0.6067021461866138,
            0.3807258166781119,
            0.0950091742072432,
            0.6519302579483973,
            0.1447369400465809,
            0.3768570579783762,
            0.9376829093113643,
            0.06614007922558363,
            0.38431596584393857,
            0.8842261159619674,
            0.5613192921444363,
            0.8808932748628524,
            0.9700610004017022,
            0.6144252347744887,
            0.32795571909065,
            0.2077968628811766,
            0.4474606065976239,
            0.9370658217651426,
            9.666962222321107e-5,
            0.1523470538193925,
            0.2122919170895673,
        ];
        let subdiag = vec_static![
            0.3056447441500063,
            0.7456252987130297,
            0.18795399969254234,
            0.10580210815412516,
            0.3667790559520604,
            0.11312018616553432,
            0.43130959590408824,
            0.12733946348946112,
            0.7754200015422843,
            0.11462492748669817,
            0.1646228849693424,
            0.5498181705657144,
            0.8215402007359489,
            0.5549515047858895,
            0.6747472950692731,
            0.9944413348382201,
            0.9009360373643603,
            0.3080754436431906,
            0.12964878635305754,
            0.3831765057928852,
            0.8174053443325175,
            0.917121275562522,
            0.6238095863626737,
            0.1930541259971955,
            0.1083599360191293,
            0.6664213913848369,
            0.9142140073621671,
            0.8317624912354198,
            0.5792497764455579,
            0.9528256828095099,
            0.7216235869434947,
            0.7580767812895445,
            0.8659742281381297,
            0.1860180109069518,
            0.7141567274413423,
            0.9929140205251573,
            0.01480728764882,
            0.7181791146645335,
            0.6989258997781399,
            0.7444815566895207,
            0.5935818769457846,
            0.42163998690313864,
            0.9477114391316797,
            0.34502405257829605,
            0.17545845562326523,
            0.059405897876968594,
            0.1726715671891963,
            0.6980682853819796,
            0.015667055926685935,
            0.7442929546620496,
            0.09489347192295339,
            0.26294454450087634,
            0.14678124983505092,
            0.9766379153111909,
            0.5446124500851669,
            0.6951033374681663,
            0.39424735336141103,
            0.3638362437695718,
            0.8268894263436269,
            0.03569331911488183,
            0.18008375334466242,
            0.21093348750655783,
            0.6403166330072856,
            0.24151644542863315,
            0.8007941708457903,
            0.9546164335175866,
            0.564693254286534,
            0.5939417115372281,
            0.848858061717452,
            0.678305051442029,
            0.34145082438373064,
            0.018996526002347913,
            0.5350645128642897,
            0.16380607335737096,
            0.9541311617136843,
            0.8972390112132765,
            0.20620851212196378,
            0.4064444449496657,
            0.22853398038768846,
            0.5625510649455169,
            0.3292394396387699,
            0.9499985301262932,
            0.46961796034565706,
            0.8550401342925872,
            0.11331071995283704,
            0.5370408275887198,
            0.4225061626572716,
            0.7113564007341124,
            0.5440695613095045,
            0.9429119229264217,
            0.9418192314199187,
            0.4595018133979456,
            0.11997303237263768,
            0.5050075121654448,
            0.8464315483316138,
            0.6001500262169397,
            0.014170583876986775,
            0.5891290090330017,
            0.8269559647969559,
            0.727008606346369,
            0.40308085040130714,
            0.4769274878744385,
            0.5031017174901194,
            0.9653308905352374,
            0.05820643913386647,
            0.5839503387605185,
            0.12402624718791366,
            0.9808307159271583,
            0.31183256753334077,
            0.2968868830122854,
            0.46023047923805205,
            0.14833982530111567,
            0.65765248704152,
            0.7931977034792722,
            0.10422377898659152,
            0.3237615937340822,
            0.44036120748135754,
            0.8152082481801677,
            0.5657751815100746,
            0.12247213033062843,
            0.12411466645776581,
            0.8111678326060824,
            0.8659650943526952,
            0.6401522697786636,
            0.8305130254791445,
            0.6155701616514582,
            0.976003503734365,
            0.5426197331282824,
            0.3930558684294502,
            0.415333251350124,
            0.6737291989761925,
            0.9073237675548902,
            0.5576736295634749,
            0.8829065362399116,
            0.8401069643961545,
            0.44484143164409407,
            0.8945051891278987,
            0.8324315271287057,
            0.37572810811171,
            0.5311645816253128,
            0.5410183184009483,
            0.19744166096326343,
            0.19842870677648106,
            0.5879941475040675,
            0.9786993854633442,
            0.5968620552818462,
            0.5135705169514763,
            0.9090223217449162,
            0.44535947745956805,
            0.3300597048965267,
            0.715628098634161,
            0.9203855120442683,
            0.4774988986047791,
            0.5589992822372473,
            0.186736834799486,
            0.4008272532964271,
            0.3954667759754853,
            0.6375743442561205,
            0.7570893839877972,
            0.3132382034582841,
            0.939759448592276,
            0.13880950488085908,
            0.8326450074085596,
            0.5435488869084204,
            0.8871764967336385,
            0.5104803787523359,
            0.8183761212207235,
            0.09841598011855401,
            0.7787772646287051,
            0.025782098684299593,
            0.1582336677185774,
            0.47189096656959695,
            0.6411639191498966,
            0.9121043592216188,
            0.8836178727290614,
            0.8619350562353174,
            0.35930466893572144,
            0.037447833449754,
            0.6479060568978083,
            0.11479409296465548,
            0.7151813234442181,
            0.8774288509162476,
            0.5088504695085865,
            0.30086979283431325,
            0.6783177195786527,
            0.8487535314833418,
            0.3917061519411893,
            0.1529037805915775,
            0.2794436368037698,
            0.3867049568857387,
            0.95028935609001,
            0.668297605120564,
            0.4091293928635532,
            0.826787369452402,
            0.49502893536128045,
            0.9204313334180478,
            0.04227152077501617,
            0.8591242352702294,
            0.08034515572168355,
            0.4016398475609876,
            0.552956667354517,
            0.995434784708499,
            0.7916465831808283,
            0.4431013751392342,
            0.6671168551178643,
            0.18963684541621384,
            0.9408021548483484,
            0.39524245248420486,
            0.4398351316404564,
            0.09865745210171994,
            0.34529364209770197,
            0.7896598088250412,
            0.620626100417164,
            0.8944054172862875,
            0.08461446884599422,
            0.5738108783945065,
            0.9234462345123426,
            0.4666998582752204,
            0.3511458700743658,
            0.687981064083925,
            0.5857159194448827,
            0.7796140288522504,
            0.7053391405697281,
            0.28374733052199586,
            0.2077966875285202,
            0.47248703407139336,
            0.10889214016591153,
            0.9907974152807177,
            0.5277388644238672,
            0.25542631824829676,
            0.6096386554262448,
            0.4853875369586189,
            0.7758278847547917,
            0.6503482206015344,
            0.7872521250255423,
            0.5835673769403026,
            0.054438190571200584,
            0.07841186389369081,
            0.42435351797029797,
            0.15148954511964963,
            0.013217228657095292,
            0.20321355812331443,
            0.41134865019643196,
            0.893762124482854,
            0.04578706985005354,
            0.7631748707976872,
            0.057836476103902634,
            0.7696513764038294,
            0.514646143350958,
            0.3699021639343091,
            0.2740557035261182,
            0.08325942766122896,
            0.21511166645435398,
            0.5173831302175944,
            0.37558859246961807,
            0.9425750468539488,
            0.14280581796726732,
            0.551969939510297,
            0.01932819087738935,
            0.5728385856388669,
            0.8283348728460406,
            0.07934738138747399,
            0.9705733034919944,
            0.30600584387572205,
            0.20438950881156082,
            0.6570611977784478,
            0.431894419621217,
            0.21644201229949733,
            0.2357002275663601,
            0.010031862472789643,
            0.7852407949785581,
            0.47591278164194006,
            0.017083534404552236,
            0.70441920699062,
            0.9879201237833972,
            0.2661143719211483,
            0.2933730098258043,
            0.913911512160399,
            0.5924086218087098,
            0.15018476931849045,
            0.7090500894817965,
            0.1578831976360242,
            0.37364308551885306,
            0.47221324235540074,
            0.2080799934617963,
            0.5960336468592128,
            0.0024262366029996763,
            0.9686573315244363,
            0.007790105364738564,
            0.7973099957337009,
            0.07772308814111217,
            0.28058789167890674,
            0.16115336259960567,
            0.27509642154779956,
            0.6947525070182677,
            0.5369825146778103,
            0.978827946469132,
            0.659858378865912,
            0.6527252000429293,
            0.7041139810749213,
            0.2869627484454058,
            0.33308842879697975,
            0.44159455295268457,
            0.27216835488560753,
            0.3393119948713913,
            0.7648329706153236,
            0.6966637044522928,
            0.2025159847212873,
            0.6374963882353294,
            0.4530678605483741,
            0.01198799668408701,
            0.8501904802162441,
            0.31602528613237324,
            0.5743196328856983,
            0.2041484874611329,
            0.31698102697078223,
            0.7251656050864485,
            0.18523041092012338,
            0.6475193739002286,
            0.4539029450472052,
            0.9443982414985209,
            0.3590332826624424,
            0.9670507123723026,
            0.48731607627716933,
            0.5297020372301194,
            0.5214213268459551,
            0.5455630655556016,
            0.28708268593677544,
            0.9311414177391263,
            0.8950938351965099,
            0.16123011234222318,
            0.22098266484499085,
            0.7493059992396844,
            0.5712912493855609,
            0.7748724242424189,
            0.1739137089632864,
            0.3759998878743801,
            0.43548864712056934,
            0.355953178481756,
            0.2934045169930304,
            0.8731884218145788,
            0.03932032739156133,
            0.730978570503939,
            0.8963555774338611,
            0.9235650437208576,
            0.019397306498324163,
            0.490413428295972,
            0.2280813710984807,
            0.3186296331862041,
            0.8588266145518306,
            0.827949675823181,
            0.04632864897129574,
            0.04514427058718651,
            0.6937002384358558,
            0.7248161565262558,
            0.48925517507535177,
            0.4218869798777849,
            0.05192442176785239,
            0.34126462267735147,
            0.07526076288545036,
            0.6615484694091124,
            0.3858483293934476,
            0.3040038827852344,
            0.45193192552327277,
            0.9725483191206692,
            0.9658066630850666,
            0.663806767413649,
            0.8982898058299352,
            0.47055757592740133,
            0.9177236633247238,
            0.3121548993263339,
            0.3278465102083806,
            0.4559364970566179,
            0.9335005702379894,
            0.8765771154233144,
            0.7446961762075289,
            0.5226080548332727,
            0.5239841658992287,
            0.7053040960804962,
            0.9747877890656634,
            0.30076239044021935,
            0.3635336573288519,
            0.25239032264109484,
            0.851046363997222,
            0.4102002699352153,
            0.5376447442499298,
            0.29695939161672513,
            0.7692080830680088,
            0.8077862697669561,
            0.8231780782958322,
            0.14925121737806113,
            0.09895355062061273,
            0.04238351423906839,
            0.6795101515783823,
            0.1836252581270097,
            0.5572449717764562,
            0.3448496904665723,
            0.8941305356130093,
            0.7754476123893801,
            0.4437958505318783,
            0.8131328674103127,
            0.3414329616729884,
            0.11087369270133707,
            0.17316839004945794,
            0.20096665035614536,
            0.25528470534727865,
            0.6186892404700086,
            0.2776263570822872,
            0.3917028828828043,
            0.6853848690727818,
            0.38794287559658314,
            0.36733234794384995,
            0.030467194704015377,
            0.13556571798486705,
            0.114452983143975,
            0.21227875846915634,
            0.7440182663283812,
            0.9553443480741944,
            0.8754730300219125,
            0.7362659679257295,
            0.5168804142498504,
            0.9278764580227046,
            0.05667135817563429,
            0.7977067629053805,
            0.6366757931184828,
            0.11550601919616832,
            0.7260010059519495,
            0.4258311238966114,
            0.14344953690709206,
            0.6854867267306801,
            0.5597128337100352,
            0.04519280943731574,
            0.3051578732289518,
            0.6941951391481741,
            0.7335129606264628,
            0.45287668627794686,
            0.8023458563642463,
            0.3355089550382604,
            0.23690241421025737,
            0.5865628088550219,
            0.8070294117926576,
            0.7124386404922848,
            0.8508405528573492,
            0.38998403890301847,
            0.815015129748834,
            0.4835569836615241,
            0.3794810774642573,
            0.4301823167622735,
            0.5777686495434117,
            0.0003715282384250118,
            0.7751840931504775,
            0.6848118511089216,
            0.3321502477454171,
            0.5726173363219021,
            0.1102768963346471,
            0.2050268517435817,
            0.8411717246701556,
            0.4359633005450527,
            0.06980411443387724,
            0.2276405768217764,
            0.4242564784378763,
            0.11031511244150671,
            0.35550185167373227,
            0.15961222186795354,
            0.8973558153349923,
            0.901699256812279,
            0.6109656303963225,
            0.15705655447271927,
            0.6039864598974551,
            0.4803097140654581,
            0.6886961751362133,
            0.5145445724508422,
            0.3885304600460471,
            0.06320845270286535,
            0.7926903801117495,
            0.17244112950216717,
            0.37968586799778037,
            0.5285337631120802,
            0.5581469584064788,
            0.7606546883505797,
            0.9801350487476163,
            0.17601211683642592,
            0.23016998819143686,
            0.9988735128922861,
            0.9964709310114106,
            0.3003878806236101,
            0.38715325920437826,
            0.8933941630898731,
            0.04044891940291784,
            0.2912809677735896,
            0.2048054109429659,
            0.8974107873594714,
            0.9015098002883342,
            0.3127354083014837,
            0.12022010780426284,
            0.779603168492849,
            0.6226056705611006,
            0.9501761800681467,
            0.282754714138722,
            0.3587714367899729,
            0.11307089195455344,
            0.6846512481308641,
            0.481035669443018,
            0.47720857165909913,
            0.2989436500983975,
            0.04195958303853875,
            0.943795202215035,
            0.7404344642271719,
            0.9200687466415679,
            0.9530475209590279,
            0.051576771787694375,
            0.37071948953905287,
            0.3023620352339571,
            0.8547931843849371,
            0.08280654010989841,
            0.25224156676090626,
            0.04676323990591791,
            0.9514986278924559,
            0.8873782411977091,
            0.34813698418421946,
            0.6888427139542841,
            0.9795211279872262,
            0.47563598498364135,
            0.8510891916037336,
            0.9957574580691713,
            0.8789164335285607,
            0.13899143629846944,
            0.33114356066954376,
            0.9804934223443615,
            0.5127871503741248,
            0.5491200087939495,
            0.12851835578042703,
            0.39361881630527806,
            0.407189498993495,
            0.4115818275499319,
            0.8534685193082066,
            0.3367047428929839,
            0.3875080369444178,
            0.3471177549950232,
            0.5410567225309034,
            0.016101658822052722,
            0.15343556374742873,
            0.6038258644435197,
            0.848910592498697,
            0.5440969764396003,
            0.7036065942941163,
            0.9363074855883752,
            0.690762068697277,
            0.810502996960463,
            0.9021388042924308,
            0.41061513647136283,
            0.3760313352802984,
            0.6534842342139258,
            0.17245604214887256,
            0.6545305414026276,
            0.09828309819907222,
            0.021347300043353834,
            0.06625066534939339,
            0.5811737328445331,
            0.2221369839526659,
            0.03386026217665872,
            0.5120593901028011,
            0.21604187959069587,
            0.6568365943735496,
            0.9228807070509343,
            0.9692443403037638,
            0.5039428835203609,
            0.473312973525324,
            0.8989304510416462,
            0.2893830738769403,
            0.5400512220559429,
            0.8317662543115151,
            0.3623927368486186,
            0.05592363514496557,
            0.2013084863718314,
            0.3511849575792956,
            0.04681970618106457,
            0.7438759357097672,
            0.40428289351850066,
            0.24886656470306912,
            0.41530975264763426,
            0.27360365999610137,
            0.18264151057750744,
            0.12633567080262964,
            0.14720341595628206,
            0.5963412693934422,
            0.7451023325387626,
            0.8403064154125609,
            0.9113802628195199,
            0.7925361242043578,
            0.4700198681208524,
            0.1624131121674216,
            0.1842174547458587,
            0.27103286978547636,
            0.036913276711137644,
            0.794120002199789,
            0.7847477377353473,
            0.9847126875015699,
            0.0604246807941794,
            0.1020765263813116,
            0.4001432174450992,
            0.9029570047752941,
            0.9935574220017901,
            0.08496185993123218,
            0.35193678925047556,
            0.6650825896044992,
            0.633123113949017,
            0.12042379186706065,
            0.7267519470749007,
            0.04960213723225171,
            0.8003428563837104,
            0.7105055279962602,
            0.6389089164725457,
            0.9327742233980036,
            0.5161833987704538,
            0.4119267138012994,
            0.7355326371619856,
            0.9258725256400323,
            0.2714885046018005,
            0.3078641242003942,
            0.4641400556798959,
            0.8967994682075197,
            0.09741890119083885,
            0.6961235966679272,
            0.641309602331906,
            0.7589672431976652,
            0.9460042050819939,
            0.16457408744224433,
            0.5905166066735666,
            0.0645854262660579,
            0.9225895487625081,
            0.8453244898116592,
            0.5383869429327891,
            0.8884890274777821,
            0.4197319265036752,
            0.7500156983166194,
            0.9891042589881829,
            0.01985050325526294,
            0.7134241950971687,
            0.3587169082881143,
            0.7525893263163326,
            0.6953390708586171,
            0.03232330711058795,
            0.6353086545865292,
            0.45124341761979414,
            0.9769930600025036,
            0.5669232572468578,
            0.938668658106906,
            0.4338164100168044,
            0.11222921731792379,
            0.03797720384967085,
            0.09844857763256376,
            0.11518910692815554,
            0.49466900134027825,
            0.3983581902084776,
            0.8316429889064332,
            0.47312999251333,
            0.4048903290459912,
            0.7884840506956975,
            0.39816934657167413,
            0.83333182620336,
            0.023453054038727217,
            0.3375802902232228,
            0.32676656023392314,
            0.8232734767141012,
            0.25256746203294367,
            0.7700913406909243,
            0.5389695844493064,
            0.5681167784974525,
            0.22419003095004353,
            0.9035474024664304,
            0.7863241515251058,
            0.3626483309717664,
            0.02491911392573798,
            0.55146177634034,
            0.21720842632939563,
            0.3041317541664137,
            0.9531364351621072,
            0.8973796968943464,
            0.229000671250574,
            0.8402370193406613,
            0.022037895183599443,
            0.836737917765931,
            0.6546192476595766,
            0.43845846524390697,
            0.18287290874651896,
            0.39722304854774637,
            0.3725183902897248,
            0.6359875693255428,
            0.761674061561912,
            0.9121528380976099,
            0.4037871092328117,
            0.07153474113947322,
            0.16417634442759,
            0.22179410578287384,
            0.6442255610401365,
            0.09656417127999872,
            0.9601509654146453,
            0.4733888258175044,
            0.7439657611528108,
            0.9675530630038234,
            0.1510564854296319,
            0.04369329308286296,
            0.309118725372204,
            0.623425445598849,
            0.29021374730693483,
            0.6766726069997415,
            0.8259349890902644,
            0.24871023553950133,
            0.5074934477683689,
            0.5856137161892475,
            0.4243963244550426,
            0.710578563910339,
            0.18834193797214582,
            0.6503852885476976,
            0.615751995056994,
            0.5014273566071713,
            0.6645690595539534,
            0.3405948211103986,
            0.19711784980950442,
            0.2998398002751148,
            0.9195616191263201,
            0.1505262293424886,
            0.8751730466670284,
            0.00834168424231696,
            0.025827354362135457,
            0.5824682659644557,
            0.15495960696894429,
            0.6559559366086971,
            0.6772137725181362,
            0.5636153937421962,
            0.7251286869551127,
            0.7726158652102284,
            0.3192164017737723,
            0.006218550100633324,
            0.8431317973283949,
            0.1425622398238403,
            0.0722025483794001,
            0.15911713561273344,
            0.3956011147755507,
            0.06941113474263982,
            0.12008503151088745,
            0.9082094438076037,
            0.6178024403175922,
            0.4235734206741463,
            0.15946541380135226,
            0.9290456481503413,
            0.7417571225789505,
            0.15009156352315944,
            0.6588396380864261,
            0.9080493138910737,
            0.1594600252406626,
            0.14932023883763978,
            0.5107192256238069,
            0.004524080201668057,
            0.1319270564403211,
            0.3453777437326653,
            0.8575218402523684,
            0.9720797451699371,
            0.721050556067865,
            0.719486529832955,
            0.3355247320271999,
            0.4320398768281565,
            0.1388342354348916,
            0.713952178822861,
            0.09473434985030837,
            0.16170333007911564,
            0.45399893003858427,
            0.4471522975096025,
            0.1620629777947441,
            0.555120679776045,
            0.9913221164835081,
            0.9916262640284876,
            0.6656790994905667,
            0.1584319326383905,
            0.7772315732793612,
            0.04950106833674606,
            0.6168568525923946,
            0.8850085432507142,
            0.6008569436635426,
            0.28434403407848496,
            0.5222091024862929,
            0.3289136157327879,
            0.8242517958509156,
            0.0014543544186828017,
            0.7055903162855025,
            0.6918733037053658,
            0.1215286623156362,
            0.606625560777267,
            0.38474228310690695,
            0.5561028312732113,
            0.6925434201360956,
            0.5030049415767984,
            0.4373757067462809,
            0.40262470465620004,
            0.29679518675170746,
            0.23443397779709685,
            0.9401971220147833,
            0.5022479785631987,
            0.8756315599830912,
            0.3566036956928199,
            0.17094319362349586,
            0.23332990304574608,
            0.3974251403805953,
            0.6744086868924933,
            0.8963282849932238,
            0.6278592405421798,
            0.03448142439565771,
            0.9835666101928536,
            0.4157584014025524,
            0.006801090241055574,
            0.2259553681297769,
            0.041359086533867884,
            0.17851980455078198,
            0.5069878837056208,
            0.387536883241537,
            0.2762301107628514,
            0.6649724905761829,
            0.12499462696151864,
            0.22684860045680122,
            0.41735033027464374,
            0.17412791593945343,
            0.6701391576784195,
            0.5307914849082684,
            0.40813851781982124,
            0.22390129513353285,
            0.005649833428942208,
            0.4883481964270028,
            0.8688437714062154,
            0.0935682881726475,
            0.12094810540039058,
            0.34318168456821574,
            0.8382723727680698,
            0.8198775509442384,
            0.7032040540890907,
            0.40973147740347793,
            0.8882182101776983,
            0.2593416896520184,
            0.7514215657589697,
            0.9891040983618512,
            0.5365707653429487,
            0.5337413102321141,
            0.06903019106136221,
            0.03681902345797561,
            0.3714071649975875,
            0.5895822360285221,
            0.27855398075340465,
            0.07192639886602326,
            0.8219230883311229,
            0.42371556956209966,
            0.9923543662319326,
            0.8757205988037184,
            0.2495530315044605,
            0.8918477804095697,
            0.8102897369370692,
            0.6608053584890721,
            0.6334165418357576,
            0.7346001037710248,
            0.3531776159961877,
            0.027802332060074764,
            0.29596932972525725,
            0.6611550391042507,
            0.05971419088778107,
            0.5336938252898652,
            0.6474078218411075,
            0.8733833124315973,
            0.27064285916942077,
            0.6800043429248648,
            0.7797586491845876,
            0.09633675865715385,
            0.15677008169215234,
            0.23394539969965789,
            0.7323421797485367,
            0.2695584209878511,
            0.8923611906456285,
            0.009384104149009809,
            0.20087989891307956,
            0.37650593207094696,
            0.9675672321088212,
            0.2460276680731922,
            0.8829436148568359,
            0.9011339590969748,
            0.9893348013640839,
            0.34402326744018086,
            0.2918830938824284,
            0.032443773270217524,
            0.20236048346275737,
            0.10644854768113488,
            0.39314962067289483,
            0.16844774292617437,
            0.0193640642109717,
            0.32131633915309843,
            0.9031289996826208,
            0.7996900072509187,
            0.9544912609903554,
            0.21238118940953343,
            0.5588780213505607,
            0.359423748193081,
            0.14306783265825895,
            0.03646714126807182,
            0.8108290011843635,
            0.5024978136603396,
            0.30684702940974795,
            0.08078866745769053,
            0.584846202681692,
            0.9731481619668385,
            0.015849082319753238,
            0.47318193572395295,
            0.45751317022352533,
            0.1347836174790057,
            0.07869729277442472,
            0.3730223469708691,
            0.8108036315360886,
            0.6191676048612971,
            0.3026033801925956,
            0.12952357665540415,
            0.9899082523324236,
            0.7427677782218289,
            0.5197759491989283,
            0.9967841038515952,
            0.5035569325914083,
            0.3834211729816368,
            0.3309587035330964,
            0.05340317428058894,
            0.6463414755064464,
            0.5887364268384743,
            0.1394604722694749,
            0.457951772941251,
            0.45288455141492356,
            0.08565262580204513,
            0.6084418050718142,
            0.8203820932095913,
            0.6461410253957783,
            0.2280002938711736,
            0.30887824900482863,
            0.9628221732310465,
            0.44209913122842126,
            0.35857995161780576,
            0.07182970907223107,
            0.171047907339957,
            0.24878787204659314,
            0.06756423474769113,
            0.8732787479160486,
            0.11342075749834801,
            0.9967087053746094,
            0.29132559183022533,
            0.7537365927002493,
            0.29658308252612886,
            0.8269346653596071,
            0.8714825210553282,
            0.07360104666475487,
            0.33337023074576533,
            0.9799778581582539,
            0.08375046754795701,
            0.8660403417189309,
            0.28286840561868565,
            0.8252295280561422,
            0.21889588864658394,
            0.003964186364546207,
            0.027000390024012777,
            0.4606990121656196,
            0.3297646362154407,
            0.8929719909234096,
            0.18414480606970096,
            0.5377843931338276,
            0.506843731844477,
            0.3338619626598003,
            0.300351599526833,
            0.1808568608406541,
            0.41477945200762745,
            0.5759119192571923,
            0.11284300062398211,
            0.749403315530038,
            0.3859482139006477,
            0.6051165122948982,
            0.971891394749644,
            0.6605478455153012,
            0.5861700067837108,
            0.07225848311996741,
            0.26352858091537257,
            0.8742611615172913,
            0.8420666125054983,
            0.30095829631010684,
            0.9035078353304578,
            0.6468305240161736,
            0.40631323088985216,
            0.26893323723422446,
            0.14348472814734614,
            0.8446063514723486,
            0.8841258110212956,
            0.259918014410066,
            0.6701006927979326,
            0.9864420189950698,
            0.7476246814792673,
            0.9505952494844244,
            0.3799973827243003,
            0.5547180102231944,
            0.7105616395309934,
            0.5383483239781777,
            0.31000439759468,
            0.7059163158915425,
            0.6856815369544714,
            0.39866345825125704,
            0.042513276582041515,
            0.337803158592229,
            0.39465691436662753,
            0.40306375626322977,
            0.8235182077717283,
            0.6090460629372503,
            0.6764030144841834,
            0.9635539428291812,
            0.7042014067502246,
            0.5127830179290652,
            0.9131677796050657,
            0.5311464655158742,
            0.8535060094374574,
            0.6649277719671861,
            0.8158423948021637,
            0.3003102858044917,
            0.9543186871563114,
            0.6693534602340823,
            0.9280256323955216,
            0.07378036482014472,
            0.7734809173963871,
        ];

        let n = diag.len();
        let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            compute_bidiag_real_svd(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                40,
                0,
                f64::EPSILON,
                f64::MIN_POSITIVE,
                Parallelism::None,
                make_stack!(bidiag_real_svd_req::<f64>(
                    n,
                    40,
                    true,
                    true,
                    Parallelism::None
                )),
            );
            Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n + 1 {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }

    #[test]
    fn test_svd_1024_1() {
        let diag = vec_static![
            0.4841629363684429,
            0.09500211557292393,
            0.09054687338157585,
            0.9349389484044085,
            0.2263540442491473,
            0.9883453587159688,
            0.27611412994054574,
            0.6934210307229077,
            0.7122722731446822,
            0.5764931807281632,
            0.7220922855179277,
            0.7690715441721571,
            0.18151810699724935,
            0.9703738941665442,
            0.46756918888654386,
            0.13829262641884732,
            0.26785351679939384,
            0.6826209585171479,
            0.7450857828877535,
            0.9612828629996442,
            0.8597056636877132,
            0.3147189885973717,
            0.9873882525578956,
            0.6390222666607266,
            0.6720524846258185,
            0.5710278204457248,
            0.012892307681411919,
            0.7804378611928872,
            0.2207373153565294,
            0.6251662057647014,
            0.13965841961139402,
            0.2611627557457328,
            0.21717079350115864,
            0.43746631711094563,
            0.3551960866238878,
            0.6076465429515558,
            0.10918455630762303,
            0.45967703949396843,
            0.7809114058971589,
            0.9954329677023613,
            0.6578987749953844,
            0.4524030893215839,
            0.8561365938081589,
            0.2702994410284004,
            0.5910189554474038,
            0.9215907321467688,
            0.7637726604998021,
            0.9996866856048456,
            0.4508665111686113,
            0.8440519119195404,
            0.5636981910772803,
            0.19691331749849916,
            0.14415353894000804,
            0.5491145376147477,
            0.5355242003504422,
            0.3569416212159129,
            0.7716747444655886,
            0.6481040816879275,
            0.0861928588345302,
            0.5644989365621479,
            0.4595649555376554,
            0.9924614965479639,
            0.34457880027775034,
            0.2709464498548626,
            0.6175995628458563,
            0.06410776075791469,
            0.8668177074089747,
            0.5133288724007797,
            0.26197543298622306,
            0.636716245285106,
            0.07784169215536019,
            0.23103012381763322,
            0.820507398231268,
            0.023267897611993882,
            0.9922903501796004,
            0.28413038646415656,
            0.4902041468658134,
            0.5353728340280559,
            0.39068757308715574,
            0.24110407030206493,
            0.7982266250190029,
            0.7498583088545846,
            0.8054303600922323,
            0.727152156075704,
            0.4433406657279716,
            0.11813500917759767,
            0.19322091867414604,
            0.6073386848510687,
            0.1892620575417534,
            0.9276884465180404,
            0.07416066041374547,
            0.11055221860974573,
            0.02742841195102952,
            0.9046475741926477,
            0.33798444110426407,
            0.25146251279015375,
            0.32543843780189374,
            0.23502095326757122,
            0.8151869673056854,
            0.06550480813538595,
            0.27213001583974805,
            0.2645917156255321,
            0.5965380863001157,
            0.6850808677017469,
            0.5485690599538193,
            0.7758605807485486,
            0.2819101125296871,
            0.21744462405804543,
            0.3210524068951949,
            0.8219967960710458,
            0.7168902509705798,
            0.20198849159812726,
            0.886790290413851,
            0.8405592175465555,
            0.6418755287944,
            0.2717157722359368,
            0.8785193775058516,
            0.2883033828479482,
            0.10560195836536401,
            0.9457172912478532,
            0.24413132566350226,
            0.4798067689848272,
            0.9758153512537662,
            0.9461639347527152,
            0.5015824600544672,
            0.4743546024442775,
            0.9439300463479419,
            0.8210197093977561,
            0.9756341250452755,
            0.3034443196917138,
            0.800696521458751,
            0.2469174591716501,
            0.24844908962357637,
            0.6729189361923051,
            0.34976823478261343,
            0.4548480895382889,
            0.06654283936888206,
            0.0538242709114648,
            0.6604648475989987,
            0.3761972351630566,
            0.17973288886807337,
            0.7785638677716654,
            0.3078309007281408,
            0.34383089894111674,
            0.9740115657118916,
            0.4142242073362077,
            0.46788525339663745,
            0.5102644222807242,
            0.6821466796495953,
            0.9234699208966476,
            0.25792002693706884,
            0.4473866152881345,
            0.5316752542174584,
            0.6753097405426745,
            0.9875155323493832,
            0.4846734070202009,
            0.25690620257834906,
            0.17444353295771786,
            0.1459026770332964,
            0.03789979000177324,
            0.009118098382703943,
            0.3369152724463358,
            0.6194036398104508,
            0.8802810559789717,
            0.5818695975885244,
            0.26902985408430724,
            0.8102535632453027,
            0.14367013617397928,
            0.5154528225856342,
            0.5525644118916562,
            0.11068694771654464,
            0.06648831260453159,
            0.30229383062322257,
            0.11043335057841508,
            0.7879174452882572,
            0.6847400055584101,
            0.7093025409825697,
            0.5672680847728112,
            0.9220621299078362,
            0.2308678094100194,
            0.11095431934483901,
            0.5076880016394446,
            0.18043287756259263,
            0.3633969552240899,
            0.0074109593813797,
            0.09540490121094358,
            0.5571906415926369,
            0.43964175899243474,
            0.6761714080491062,
            0.4050321473554952,
            0.4602021774929316,
            0.07357681703454355,
            0.5647139088824532,
            0.6632368351901867,
            0.038928065259385525,
            0.44302015465469524,
            0.30679321194639764,
            0.7896327936646736,
            0.36808308055633976,
            0.9630604740173925,
            0.5525981053006727,
            0.4390151783304377,
            0.5986597671093334,
            0.7053735351729479,
            0.3744694351975014,
            0.06532632305974129,
            0.7215457382122921,
            0.008636006836127419,
            0.6280218545206359,
            0.33553959659277455,
            0.25002369784269374,
            0.2222248299935472,
            0.4568576034259546,
            0.4735434309364681,
            0.7560202412217341,
            0.2878333331705232,
            0.8562148011742544,
            0.9300668918750148,
            0.5038181128961342,
            0.8752815415361238,
            0.33283717808771585,
            0.0533181654462932,
            0.6436829043555116,
            0.6420080671552673,
            0.35854475824887166,
            0.27660688607425366,
            0.09673817671242757,
            0.7732513019446627,
            0.8411109662991648,
            0.749581543720349,
            0.9699796229928836,
            0.3521026232752017,
            0.7805969271105201,
            0.488079532732169,
            0.3153834803216925,
            0.07515270460745327,
            0.9732385585699602,
            0.4379249859226594,
            0.47941135538293433,
            0.13531367195050192,
            0.7146259058201841,
            0.4191544792989341,
            0.9918711123891871,
            0.30163052510779575,
            0.15398133758394816,
            0.8062176573881823,
            0.48038219879486577,
            0.3693644458963321,
            0.2627309145392913,
            0.7075668012609716,
            0.4565937211789599,
            0.5928922749230827,
            0.3384720695832677,
            0.7510771063489731,
            0.09656778467148652,
            0.6111109196994062,
            0.4147274155510545,
            0.0825902347020141,
            0.23342446109854698,
            0.8755256481911295,
            0.3410479507631834,
            0.49101534840303174,
            0.9696314869369961,
            0.510646800941389,
            0.21155499454128557,
            0.04077883012959882,
            0.38616102006555564,
            0.27156802107462563,
            0.06772016667809999,
            0.08411279644339542,
            0.8105121747384336,
            0.8226062374641675,
            0.44436768427765405,
            0.8725783369785883,
            0.779280645376425,
            0.3493455256878125,
            0.8904257905232991,
            0.17885649868819264,
            0.37091811243722606,
            0.9379307789143213,
            0.5731629435054184,
            0.24143821812058197,
            0.4049562680058758,
            0.42299735446198383,
            0.2527001487162306,
            0.9054521922102531,
            0.8277991529134413,
            0.8896621223418875,
            0.2839601147382753,
            0.46967983028760474,
            0.6504612353037362,
            0.34359028291120974,
            0.49029076200099064,
            0.14605930621786978,
            0.7971485298165085,
            0.5670859996593979,
            0.5839996800579184,
            0.6921622958466791,
            0.013697871099110137,
            0.5716412855888595,
            0.33906402404202585,
            0.22346066558462552,
            0.3951530817090858,
            0.3137771706646575,
            0.17589811380719156,
            0.6129863037725723,
            0.5529667540622067,
            0.12509740175859485,
            0.08164145825063607,
            0.6066900494595772,
            0.841451822666421,
            0.674802798921404,
            0.6185795283245251,
            0.9582067789561727,
            0.2569207105730952,
            0.6883723276402567,
            0.9437634272458615,
            0.6907567856518468,
            0.30993500261298346,
            0.35730588644284866,
            0.03729512159713966,
            0.6212835719526415,
            0.9352121927645822,
            0.003243518888762509,
            0.5186143509666988,
            0.6353445184976869,
            0.3617368415649064,
            0.0490224440259972,
            0.050915012421679506,
            0.2242457283292124,
            0.8147096772905736,
            0.6409998571108203,
            0.4429200379026843,
            0.9765405935181827,
            0.947474779514954,
            0.18982873752274343,
            0.9001090977751718,
            0.24092805773510084,
            0.32038670586129525,
            0.7243688544237985,
            0.16277724068208277,
            0.7593379678662593,
            0.03968818460330925,
            0.7244607120881216,
            0.5165074643832736,
            0.5509416401806246,
            0.27586276721428116,
            0.21355188867529862,
            0.6848900102063389,
            0.6036452778970051,
            0.24640329117263937,
            0.3724137096019503,
            0.6206648555568799,
            0.7958800469169095,
            0.4900216823225789,
            0.7667675482271813,
            0.7289915357163391,
            0.1869296356076564,
            0.4524808232640244,
            0.7127991694765413,
            0.8523560180647201,
            0.08452399270145938,
            0.08783929331698737,
            0.9187616736013637,
            0.8344792646462194,
            0.9271740035408323,
            0.6370044327004926,
            0.873942787396045,
            0.9326891534142052,
            0.583677916072454,
            0.5922397691297683,
            0.034166387868455494,
            0.49377929786225716,
            0.06968856674405388,
            0.3615198138197385,
            0.6147672290102518,
            0.7483870374797253,
            0.5383849201838203,
            0.20836389375701803,
            0.051848999025362885,
            0.7320297267131506,
            0.924153708258932,
            0.24940540066017358,
            0.7254900302438296,
            0.8398185409002303,
            0.777945701086813,
            0.6028385015259239,
            0.8381119231050299,
            0.27366756479468357,
            0.46285049762980857,
            0.007202203008645491,
            0.20677467863560528,
            0.28019168675955963,
            0.6564077343354984,
            0.8408712038064403,
            0.5349118002964522,
            0.43356705996070866,
            0.5525629375884271,
            0.8029332651737107,
            0.3789630801802527,
            0.17394534806832818,
            0.988718962514845,
            0.008255988120142943,
            0.015895226625594283,
            0.6206393567272559,
            0.9209847878321579,
            0.39110624455115695,
            0.7437450370797415,
            0.8941157743879106,
            0.011860557459840382,
            0.4065207233329732,
            0.23282699080782854,
            0.4136910892382927,
            0.7206841713816652,
            0.9464858269922742,
            0.9756111958533134,
            0.8903027302325448,
            0.7803498010740468,
            0.8488613163761867,
            0.07892513274645052,
            0.30522172054483054,
            0.4993261339902775,
            0.1307441671901306,
            0.6724515744593036,
            0.4119707684405064,
            0.38835171094008847,
            0.2895036626226579,
            0.718862164114217,
            0.7422150014781085,
            0.577025210006751,
            0.8438049461114684,
            0.04479570356686591,
            0.006571236569574479,
            0.17682956223658808,
            0.967370326521429,
            0.6761115997966827,
            0.896496576282121,
            0.42414571451691774,
            0.2643999567897386,
            0.7674469660502053,
            0.13691540715717643,
            0.6323850606911723,
            0.722449896798871,
            0.12800291638316297,
            0.5857947189453917,
            0.4163336920317575,
            0.7884813514972842,
            0.1508727072569006,
            0.19160043939953175,
            0.23301374790352558,
            0.9359396775007895,
            0.9774923318040876,
            0.36863638896238904,
            0.6610912680758803,
            0.46057923250460875,
            0.9590829904391315,
            0.1991152018103779,
            0.28851906877515865,
            0.6000178123986674,
            0.1718467848396532,
            0.7441859910583363,
            0.4622654532527547,
            0.5555247226894746,
            0.7677773119779349,
            0.6189662457058476,
            0.6988753748655162,
            0.4750032009040738,
            0.5857617813696513,
            0.3775093165711094,
            0.44160821335373923,
            0.12090759035787657,
            0.6841147475130123,
            0.03774648075440323,
            0.8954128692100448,
            0.5935413509246164,
            0.06702705653530572,
            0.28700587543321343,
            0.0697394877285672,
            0.9125516672429073,
            0.6719964366102898,
            0.9959123901824811,
            0.504950180087362,
            0.9477042666997653,
            0.09952647180930152,
            0.23616454829702027,
            0.8089820248976782,
            0.5723582906192601,
            0.45900223107602056,
            0.7190603249230394,
            0.7312224218219942,
            0.2440096836115907,
            0.3214679621992278,
            0.6436465041307681,
            0.8041091255879695,
            0.9295477549605089,
            0.22298283586216816,
            0.9521137131163172,
            0.9113445716477977,
            0.9091525207587764,
            0.98119554906578,
            0.9623632967357395,
            0.4176562224573084,
            0.43224238227876044,
            0.8594532789544238,
            0.16938535196509052,
            0.5192280607612347,
            0.9258356358139087,
            0.9614365095053088,
            0.9978293940686767,
            0.7113526084695919,
            0.12023427192990421,
            0.6130024834770222,
        ];
        let subdiag = vec_static![
            0.7089936750607991,
            0.4483258132447713,
            0.9981541517505396,
            0.4357762689203716,
            0.08785321714493932,
            0.5830593471239359,
            0.8920829832035405,
            0.41879264651643233,
            0.6111872756826352,
            0.8355133394363216,
            0.9793257049723074,
            0.3077792382592349,
            0.026969652678317857,
            0.4182365345593675,
            0.7165887422928039,
            0.32051113319939784,
            0.3040695673093029,
            0.12374943078010614,
            0.5760995454778959,
            0.8349949587005299,
            0.8659898386028743,
            0.7380424842517467,
            0.3393928531765691,
            0.7692042361831338,
            0.7796769522976207,
            0.03394625254827366,
            0.8571181864186348,
            0.6332478925233004,
            0.5184070284563695,
            0.019874111122098692,
            0.34934923133474194,
            0.8996435373419481,
            0.9380698449825945,
            0.42729146961334785,
            0.8503760643862777,
            0.02100272123108793,
            0.860005740282607,
            0.5326670023777708,
            0.5375140475812259,
            0.47919176360919014,
            0.7828043406116267,
            0.3794436036617802,
            0.9565916535824568,
            0.3980599768323815,
            0.8370299250814085,
            0.0849340864800805,
            0.7982582487709378,
            0.8809610530914053,
            0.36691575235983476,
            0.33283272535856,
            0.20354784446582086,
            0.5905114553771318,
            0.9047458258061869,
            0.0741901700696852,
            0.7621042915415703,
            0.9271099058676796,
            0.9611052970248738,
            0.21963155934129597,
            0.1558133881330961,
            0.19737060099724124,
            0.6639174149441336,
            0.8533931201297167,
            0.020945116530869057,
            0.6309209706848019,
            0.662729381326571,
            0.5035887768043217,
            0.1860476975995281,
            0.7977891924110831,
            0.6120596386351329,
            0.4508756288936743,
            0.4280982611244475,
            0.6565556625529626,
            0.05492439555509321,
            0.8468248520937989,
            0.20170352295274463,
            0.7393170393911367,
            0.48708726141266956,
            0.5460887373535718,
            0.656782452876117,
            0.568726037595071,
            0.026929796216858648,
            0.4245161336525607,
            0.9785776113872318,
            0.27746635862377156,
            0.14235497780854722,
            0.5841314393096457,
            0.9132559628467195,
            0.7254633296207957,
            0.3994675683815655,
            0.6542374938945021,
            0.1902512272011253,
            0.3277110299931433,
            0.33880621714171744,
            0.7594202382313123,
            0.0741024563031083,
            0.4449907034648557,
            0.17929774764796536,
            0.12210400948657041,
            0.6787006921534013,
            0.7281232868795051,
            0.7792629004817995,
            0.08064014445414724,
            0.9724962652917525,
            0.5641512037393203,
            0.9530186413776301,
            0.7883920219250815,
            0.450092735561583,
            0.5860790804560074,
            0.6807750490420452,
            0.987717614690177,
            0.21946639995166828,
            0.3756875030946536,
            0.0017060266273737357,
            0.3528908661861736,
            0.23522785730653006,
            0.6636060441904992,
            0.9407607887743468,
            0.7781934785793816,
            0.053397574393358016,
            0.5877392899593834,
            0.36009459733262517,
            0.3946933470154008,
            0.003646320901408595,
            0.597476378931147,
            0.43062018218454423,
            0.754668884154315,
            0.7355323365994135,
            0.5992070407324596,
            0.80570947364751,
            0.8795556637529897,
            0.8470422441081501,
            0.8946910951244957,
            0.6124018879965861,
            0.7088007839992834,
            0.9492294886442566,
            0.2251524815149637,
            0.4393504360847734,
            0.20876330443014623,
            0.6237404705834967,
            0.568434233274606,
            0.9168918462734527,
            0.3169042147373512,
            0.6276703450566922,
            0.08898882215107173,
            0.7321272744066755,
            0.8346756063313672,
            0.9565539131638344,
            0.003786651390763507,
            0.9521878269851143,
            0.8212172699917235,
            0.6238752560966074,
            0.09062018280410378,
            0.5532464578105598,
            0.4732513968021569,
            0.08469215363970584,
            0.6052403882500162,
            0.9941187028387413,
            0.7991286021147076,
            0.41618648415719917,
            0.22088579734088232,
            0.35506367821590523,
            0.3195577484429003,
            0.4312013001520172,
            0.34701617555070774,
            0.02232731969316082,
            0.6455567189539291,
            0.03371765698821416,
            0.9994490916392196,
            0.38346264435008326,
            0.3900943382330475,
            0.41645227148290087,
            0.4072897445006035,
            0.49980001378406624,
            0.9624024912309374,
            0.4243441122717946,
            0.10097127806713302,
            0.27318128155213284,
            0.01980432947685362,
            0.3919061611532205,
            0.5400372964620943,
            0.2837105264404538,
            0.5103853253037687,
            0.22502206457461582,
            0.42980394628690555,
            0.4255706600799779,
            0.05882680912038962,
            0.045980087565794414,
            0.4593492445739672,
            0.16748295392635637,
            0.5815760659541308,
            0.11804087865064283,
            0.9808592457277086,
            0.08447454175042679,
            0.14528681629095253,
            0.7966020134503727,
            0.39988958241786354,
            0.3966452292589152,
            0.7632471794259268,
            0.8364344797266556,
            0.05406706032400832,
            0.6196158413543642,
            0.8134083748522049,
            0.44315471096621983,
            0.7381281011866444,
            0.18178582271615507,
            0.39598457602595705,
            0.800755518965327,
            0.0996500182280885,
            0.6928794391981185,
            0.9271894035862747,
            0.420985543811215,
            0.8299212709536242,
            0.43361990762578595,
            0.8474588422074529,
            0.5384961461705045,
            0.04579216102837591,
            0.8144388324215048,
            0.7503331242965274,
            0.7443007483027947,
            0.8591172111741237,
            0.3129572042807881,
            0.43629364584579977,
            0.040775882175336786,
            0.5265987324893766,
            0.3278197764720532,
            0.9638233406714964,
            0.593089005448218,
            0.8531017811247185,
            0.3833716681860213,
            0.734770506472232,
            0.3961906892927213,
            0.7378952710775825,
            0.37054254273069753,
            0.25018676528013395,
            0.9155778839394266,
            0.5775373957855319,
            0.9299799078638226,
            0.1559513162598678,
            0.019987346908604264,
            0.35431474983114275,
            0.07348835806127052,
            0.05331421841012918,
            0.37541187674860843,
            0.3661350181166707,
            0.46649040220621407,
            0.5391607833956678,
            0.21034606925423682,
            0.058248534604802504,
            0.25723227398593673,
            0.3993796060279372,
            0.6796992856613469,
            0.08247673723983995,
            0.023451831694683567,
            0.13695099159078417,
            0.7384727200594983,
            0.6820765144990079,
            0.18829448527021186,
            0.41752496350142543,
            0.44795608890648975,
            0.05387150818596265,
            0.09342223756050061,
            0.36871457265674346,
            0.38200253190773514,
            0.9780514611196112,
            0.8456093037811834,
            0.1013726609210367,
            0.46219518711839835,
            0.6917545713401585,
            0.038618164075749584,
            0.40274635983558005,
            0.7018794912964974,
            0.6478854695078218,
            0.7477266217538235,
            0.6155100513086282,
            0.7365073270103238,
            0.12209169396041908,
            0.3190988421650174,
            0.8152293593221053,
            0.903108385570188,
            0.7992173115598106,
            0.7778936768007642,
            0.5419106038480274,
            0.41508926093445864,
            0.6245464965280143,
            0.21031078381218093,
            0.9157838433489752,
            0.2068179324203827,
            0.6403528464071976,
            0.22552934946201353,
            0.3498260607478918,
            0.5922724980687619,
            0.38056497907580034,
            0.016171926437021922,
            0.4563335453363586,
            0.8915640222527538,
            0.9009568027990429,
            0.39953462973284104,
            0.31682222212460576,
            0.8549860892263802,
            0.3874141470807463,
            0.7162930394888214,
            0.17862862530412082,
            0.7807735411730311,
            0.6005852379109198,
            0.5788955529636066,
            0.1998735921588788,
            0.0746258058981547,
            0.6755307862214068,
            0.959465854838282,
            0.7843160832503643,
            0.25173786054792646,
            0.8678783883274283,
            0.3354412471856949,
            0.298729267655819,
            0.7240785327926429,
            0.16587799922976687,
            0.7738582127496083,
            0.055824188508107664,
            0.8474459279899753,
            0.20327555814239462,
            0.6358785119454993,
            0.9923670488531944,
            0.3927013509323347,
            0.24821353641176047,
            0.7192887012700124,
            0.3391411570681473,
            0.8611950034309545,
            0.7322169437654343,
            0.6618650095669462,
            0.30558339768650067,
            0.7952130699383725,
            0.747194229022527,
            0.7877859162499721,
            0.2628035609220535,
            0.8908939970244619,
            0.1670668384252767,
            0.3197013127719599,
            0.4346388067636745,
            0.016611764360168424,
            0.3155599914778735,
            0.9711458254001295,
            0.5669388313504983,
            0.8225768349717703,
            0.10813802446203624,
            0.5150844011353757,
            0.5546695430534467,
            0.05753382687024333,
            0.3564421503451065,
            0.38259748941509086,
            0.4517495518340209,
            0.8539300200628299,
            0.6928383607839574,
            0.662790903000381,
            0.3728097457050529,
            0.07341040804273502,
            0.7135621444049983,
            0.31494535845890126,
            0.8463389381085897,
            0.3552370271076457,
            0.5334047271865571,
            0.44829450788280034,
            0.7392049863313708,
            0.9406772837478246,
            0.8205141626131973,
            0.4750865936497728,
            0.9879160960776046,
            0.4536776307377438,
            0.498044342363688,
            0.5982072579860978,
            0.47692296384000243,
            0.482346372060111,
            0.3520092892408174,
            0.6252355284088753,
            0.7908458992815688,
            0.4951939932647895,
            0.36334123906868776,
            0.6086985051092341,
            0.767302370706033,
            0.9117783574459476,
            0.4891381026342524,
            0.1379600694717562,
            0.8358452329194815,
            0.7878489151861279,
            0.0585353932698931,
            0.4137802559208035,
            0.4964892266394356,
            0.6340343086640661,
            0.8123945627818064,
            0.4276739901862191,
            0.46116608952790905,
            0.6117911541905424,
            0.06256859422580863,
            0.2897511039044913,
            0.9575747514837571,
            0.6260440704739371,
            0.28954243876188457,
            0.5730799397713243,
            0.5402210629491576,
            0.6899624495178694,
            0.3952333785839468,
            0.419967752739726,
            0.8817800062416639,
            0.9552369145214008,
            0.9404590446314967,
            0.8209434622238633,
            0.32398948251105153,
            0.23313715271019653,
            0.635024474315615,
            0.6206876961727517,
            0.09676694992655821,
            0.8247916367437644,
            0.6633543963118202,
            0.33219769457683146,
            0.6525608003946799,
            0.010264844795985328,
            0.20339365066885373,
            0.5293482755601088,
            0.6850695045131971,
            0.1837872683403884,
            0.19661655300335612,
            0.3215943227053656,
            0.4237431770336323,
            0.7069257266285722,
            0.00014216843975400906,
            0.12789839251588386,
            0.9870745527308604,
            0.3358551482188855,
            0.02649377917959217,
            0.6037087692892408,
            0.14468946436750985,
            0.8455686901890028,
            0.5489341054502427,
            0.6416105810992518,
            0.8716835798812409,
            0.9855863643288766,
            0.7621222238453107,
            0.10971708819658998,
            0.5992404260859467,
            0.0960184757175312,
            0.7478831288740773,
            0.21281437228159317,
            0.039989908116329964,
            0.6666889228105682,
            0.7082334278192032,
            0.4965891745189013,
            0.8448474491099335,
            0.51899221270943,
            0.053533044882776104,
            0.1832037916939313,
            0.7086558266051965,
            0.09232277641153386,
            0.6600839540684303,
            0.013429106260064105,
            0.755355951658492,
            0.1521493574747187,
            0.7359029926157673,
            0.31386400087456223,
            0.2786935794976002,
            0.8574892490208118,
            0.7048444522850063,
            0.11099956938971611,
            0.41377696534215747,
            0.4972042095004061,
            0.8123024006102493,
            0.019842136393627086,
            0.9110135852923356,
            0.7333099706166278,
            0.9354587752844313,
            0.9746672511177458,
            0.49312307327819005,
            0.009712103267392691,
            0.4179387564431378,
            0.4264449180491885,
            0.3178185085304487,
            0.4568848031303122,
            0.6746225129344577,
            0.612490689125883,
            0.43797541886896396,
            0.8206451670821743,
            0.5755039811379289,
            0.6969733354065955,
            0.5409265436547918,
            0.4319598200039658,
            0.2595330341771661,
            0.7496365412646532,
            0.44554171051435454,
            0.933953734036019,
            0.6394466211797196,
            0.6253318254844847,
            0.622598526858425,
            0.42819693534628167,
            0.49608801391604496,
            0.004078043936252662,
            0.31546465727721773,
            0.5620275115036629,
            0.24981547755110622,
            0.46638455050467276,
            0.8203192948951312,
            0.9531460405175395,
            0.08767861482470207,
            0.10182665848106964,
            0.41722139013286297,
            0.9050081587484748,
            0.7997471863258789,
            0.07654727069374845,
            0.015678303100936986,
            0.7531058353281517,
            0.11733320889337284,
            0.6678888964137968,
            0.25222433839055036,
            0.39593237313221374,
            0.2639734864670895,
        ];

        let n = diag.len();
        let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            compute_bidiag_real_svd(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                40,
                0,
                f64::EPSILON,
                f64::MIN_POSITIVE,
                Parallelism::None,
                make_stack!(bidiag_real_svd_req::<f64>(
                    n,
                    40,
                    true,
                    true,
                    Parallelism::None
                )),
            );
            Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n + 1 {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }

    #[test]
    fn test_svd_1024_2() {
        let diag = vec_static![
            0.009879042533147642,
            0.25942766457357236,
            0.40170082173257604,
            0.32716377867657065,
            0.22784080238282056,
            0.8011220261693188,
            0.41034688531555497,
            0.6337482059687781,
            0.9268147331465462,
            0.9007255417991846,
            0.39500004088544705,
            0.03216097972007015,
            0.012917906438069893,
            0.7941462600953578,
            0.756551131637162,
            0.6277212943286069,
            0.4750538755815341,
            0.8050777538357257,
            0.6411193842462393,
            0.5964472181215076,
            0.7526694114715058,
            0.8570880824272364,
            0.17902227574406282,
            0.8109514743574001,
            0.5606370535254248,
            0.6598192805934875,
            0.43078033013590533,
            0.5825029672946985,
            0.6956259864573019,
            0.4651998256633355,
            0.09762364687238201,
            0.24074173129736165,
            0.7123654938926,
            0.935829517920511,
            0.11246136111668859,
            0.28663069616787795,
            0.052842680220333116,
            0.015474947998349697,
            0.7310762139439319,
            0.23933959701846774,
            0.8822425987482592,
            0.8685083697899169,
            0.652772891730832,
            0.9613456318501793,
            0.3562755742479843,
            0.3529317070096095,
            0.5203651320326466,
            0.9003075461626292,
            0.1264415938158331,
            0.08351000046655321,
            0.1753943981928473,
            0.6980433592474139,
            0.8578269024506149,
            0.6908011920858023,
            0.5674517380556917,
            0.01702871963976549,
            0.07839669995380683,
            0.47841583322083037,
            0.34526252196058815,
            0.8234110245951314,
            0.08003030493935503,
            0.8985662624131026,
            0.46734383275741376,
            0.7161188969879848,
            0.3815623699352576,
            0.9985517387444293,
            0.8492572583223449,
            0.8205777546331864,
            0.8770957313542075,
            0.9737076660054085,
            0.014235964068120666,
            0.8072818162682582,
            0.6886219904348015,
            0.4552576261137854,
            0.9974413848153643,
            0.663487101355474,
            0.12797047898911695,
            0.8730708596405391,
            0.7358872544204906,
            0.0024232499164960064,
            0.3123244065132025,
            0.9129677973115747,
            0.12381606964146508,
            0.6714861406297731,
            0.6330879630315885,
            0.15970950034649944,
            0.12990016300317808,
            0.20715056410837385,
            0.23864283500085526,
            0.9196384835185076,
            0.6786187887920997,
            0.971364503522331,
            0.4991613803578423,
            0.19656248169292956,
            0.36089468141027725,
            0.17351799213873953,
            0.34283715347954935,
            0.202274893550491,
            0.7573587290836572,
            0.28781764486060746,
            0.29286987330948355,
            0.18041818830387502,
            0.11085478108502878,
            0.9248101417660487,
            0.7196648633376221,
            0.7948623795256567,
            0.9618964043172259,
            0.5358432591346612,
            0.41078427228367975,
            0.7118268810392685,
            0.08930552972766292,
            0.3037261272140287,
            0.4873595668729337,
            0.2759135908426116,
            0.004931914769020551,
            0.9751958784750651,
            0.4803312130077346,
            0.19625162474739133,
            0.6338278104821975,
            0.2070709701863036,
            0.35289231926619125,
            0.8075051412594402,
            0.18922437321221275,
            0.8090676916035391,
            0.34692036473765697,
            0.5134105296786168,
            0.2537789123309152,
            0.31740425288639873,
            0.6914865672540939,
            0.6171757974608877,
            0.8067004627565533,
            0.5548379967230543,
            0.08923412688878118,
            0.7373239475246149,
            0.9973855333306278,
            0.7893809917271625,
            0.684211884371379,
            0.46218191430276034,
            0.17597080473258842,
            0.9632675715873064,
            0.5611078882167525,
            0.2939088572594233,
            0.1312840046608058,
            0.4074087197731001,
            0.24449749140257027,
            0.9584081457748832,
            0.598735572583611,
            0.8719134466881342,
            0.7473254819575135,
            0.43051500118847075,
            0.5680395392137619,
            0.9992003434168677,
            0.09697960853735677,
            0.7539212337070309,
            0.3135142583807121,
            0.3089605207933839,
            0.7247740236649471,
            0.4341641533676518,
            0.9738557480191191,
            0.7568112381061494,
            0.6988946241305319,
            0.8671069033150816,
            0.9199084451720417,
            0.2339808510845095,
            0.9120851645301419,
            0.52184538505289,
            0.8686042347324593,
            0.5825948463951326,
            0.3066180985985336,
            0.9893240124363446,
            0.9542060294495651,
            0.5433692697539214,
            0.07007746021702566,
            0.849506872522393,
            0.46513028331219763,
            0.5841677314662393,
            0.9388553738536585,
            0.4135859170203644,
            0.13279656262896178,
            0.9764168577593498,
            0.7121771180000449,
            0.41753476055643757,
            0.4694451918226963,
            0.5606108747640179,
            0.6921120653839485,
            0.10404989133518994,
            0.9096628373622678,
            0.6212496902578757,
            0.9039775825048112,
            0.2519785269022933,
            0.642636753005936,
            0.39313619189426474,
            0.5049087793125976,
            0.185123471096711,
            0.4738105908508532,
            0.40077831714296763,
            0.6610403139371497,
            0.16960410142423576,
            0.7720571606452117,
            0.47301034519076934,
            0.5671632092141498,
            0.058571919388965,
            0.1137817769029924,
            0.9293888916781323,
            0.7337496351796344,
            0.47872618860877636,
            0.049900525598919554,
            0.5899099081681471,
            0.04598868985641036,
            0.7118427779653944,
            0.22686684711973948,
            0.09467985694967407,
            0.828126484887492,
            0.38062807400811627,
            0.13432158358838764,
            0.8819366554870842,
            0.06076628659369543,
            0.5018549738258863,
            0.47726578024585364,
            0.6796227829227661,
            0.0307355495533701,
            0.7219689204239849,
            0.2665398161653749,
            0.7457946992965994,
            0.8014906918498865,
            0.7032050489593877,
            0.39913936032204245,
            0.344868641245594,
            0.8460367081641266,
            0.8611874056977944,
            0.28677011663765195,
            0.9236609706292132,
            0.592690189893941,
            0.24627686698958096,
            0.05327782694837213,
            0.31732511563500243,
            0.03522553387439098,
            0.5105357808439257,
            0.5300916241678767,
            0.9122917979999389,
            0.3896359534024416,
            0.8157532084708795,
            0.8165976967791265,
            0.6515888300611864,
            0.01682583396667725,
            0.8404708464497803,
            0.5019880976343682,
            0.5407503246795986,
            0.7360568473812241,
            0.6853540818970443,
            0.5247918359630233,
            0.7393887921536667,
            0.8131098769020638,
            0.04116878848575245,
            0.17884808631370075,
            0.795585260728776,
            0.8963137184542852,
            0.20078125672437008,
            0.6822915485500384,
            0.13695879926780696,
            0.2850697874999216,
            0.018404474915290847,
            0.6142587891665715,
            0.7426430268567997,
            0.5602787315688591,
            0.025757157656970397,
            0.6646633612347755,
            0.20212404501160852,
            0.08176186878334868,
            0.3472804854339161,
            0.9443673717488501,
            0.58684993396532,
            0.7981297693851774,
            0.7585078706062724,
            0.6032665234646059,
            0.0897210077358962,
            0.9947864491545249,
            0.2899798467013329,
            0.9361953260343231,
            0.2876360062375225,
            0.15022765092332824,
            0.0947873377448969,
            0.19800367758769943,
            0.08747917919372117,
            0.4949460545148765,
            0.011802102235748402,
            0.07848227267953467,
            0.7220657649901457,
            0.8143141134598415,
            0.2534052430910867,
            0.3001731119539667,
            0.12340729205709178,
            0.787317670027495,
            0.629669432731565,
            0.2880200853036611,
            0.16226451938719422,
            0.36046675429078934,
            0.1701666367778586,
            0.03453409120781836,
            0.6004559189388794,
            0.23581641485601612,
            0.5280669351043628,
            0.7548114921998968,
            0.2509906416696046,
            0.9386190839216705,
            0.5780293946541897,
            0.1309615885069001,
            0.20329455560733034,
            0.5028424085002999,
            0.2400295683326339,
            0.3143016923933426,
            0.969039206593452,
            0.367912630536846,
            0.5705945838187225,
            0.5248938705978503,
            0.29731376199703297,
            0.9502044873972788,
            0.6259073750825663,
            0.41444941678247393,
            0.28737013771372466,
            0.48705632242104335,
            0.6507400543562598,
            0.6592907477931843,
            0.4995334924295577,
            0.08724920185129703,
            0.046334012127957735,
            0.12789571448865977,
            0.7925758209411751,
            0.41177797999498333,
            0.7280349975614596,
            0.16678381826476318,
            0.6782940771926207,
            0.8241527879608631,
            0.2886575230739752,
            0.5496549642848929,
            0.20551094816275284,
            0.49464000644863804,
            0.39009807772712346,
            0.30220616569167813,
            0.7046371963064363,
            0.8947263781950788,
            0.5575751294561873,
            0.9322608164618286,
            0.741536364482585,
            0.1909643568050814,
            0.7851540896341616,
            0.5184542009678219,
            0.7893146472184174,
            0.23365714387028158,
            0.8852121789147809,
            0.14445654209664516,
            0.4588607222062475,
            0.24432012414481186,
            0.9612842447541281,
            0.45983047487849205,
            0.5523603190690298,
            0.3291724000017524,
            0.43385527333597307,
            0.3505744714212933,
            0.023087516727097412,
            0.5071839293492867,
            0.8551265527948764,
            0.41987334130223,
            0.7841528560802025,
            0.4472398086198166,
            0.6435783918893202,
            0.05825220493874805,
            0.41536222391490796,
            0.8746933502601629,
            0.2731947693472474,
            0.5305223903055022,
            0.4678374426676837,
            0.03756925534516398,
            0.2926013108528348,
            0.3084928970079136,
            0.6475766601345854,
            0.04212485331082927,
            0.5635803337549966,
            0.9650888196115347,
            0.7256947295966187,
            0.6033636731787934,
            0.03830387828268755,
            0.4822322188559294,
            0.9144517266362243,
            0.7313201755134171,
            0.030072503406533335,
            0.818875249918176,
            0.8143288731590852,
            0.8329629136784739,
            0.1944921757341337,
            0.05999952456212865,
            0.954791314590924,
            0.6968444446984309,
            0.8415438993281099,
            0.8104038752608848,
            0.9235846588764935,
            0.5071930898060761,
            0.981281045431366,
            0.01932415570284629,
            0.17232045920218053,
            0.5339798690507419,
            0.21671241307027111,
            0.9693469116453112,
            0.6804465936338698,
            0.6920768870899838,
            0.3685724389382682,
            0.6320342688659961,
            0.9485911813862122,
            0.21613777453897431,
            0.3490192603566519,
            0.4344451777213022,
            0.47322333392509885,
            0.7019421587493292,
            0.6284156144703128,
            0.02091274361028006,
            0.20753380828946955,
            0.05065441005923155,
            0.514343363311585,
            0.4382356537431309,
            0.8311461626025239,
            0.44464423311655177,
            0.07117653274382474,
            0.15834970227217948,
            0.4362212212517339,
            0.9145574773998393,
            0.35832540201183105,
            0.6792445083226095,
            0.9292244651766712,
            0.9066256192582868,
            0.7616847446103808,
            0.42116197156710145,
            0.4015473379915795,
            0.6460768041419113,
            0.2862748731816819,
            0.7020276191930521,
            0.6291602513924968,
            0.5232558433180349,
            0.8257727166559992,
            0.11513885209593933,
            0.6925901029622323,
            0.5375112983483977,
            0.5865353672412245,
            0.1444663737523756,
            0.6317046556717392,
            0.12083588293905656,
            0.7679086813222006,
            0.9467660510253647,
            0.7772848574161543,
            0.6546720940739391,
            0.09593875178740707,
            0.854691686793777,
            0.9269677437706112,
            0.6201262539631173,
            0.033532540194856075,
            0.48361037458788025,
            0.7499998154464882,
            0.9599138494945278,
            0.6373755929402666,
            0.67169969402632,
            0.3279703742416862,
            0.4952635435438998,
            0.6900526231740292,
            0.5105236806997386,
            0.1643138318654166,
            0.41587392439644844,
            0.7444604521060232,
            0.14403480963377757,
            0.830570382850683,
            0.5350012051469126,
            0.0760991476119226,
            0.7692125685549903,
            0.7130648584389353,
            0.9479603653126061,
            0.5304396418782369,
            0.20982837490877027,
            0.5718515733933942,
            0.6948247919391625,
            0.6507592130358502,
            0.6396306475898776,
            0.10592189604912228,
            0.5918255784804017,
            0.4079679176088701,
            0.15059104663989364,
            0.5671066668133875,
            0.7055056043665614,
            0.24494640874438212,
            0.25963553178303445,
            0.13255408754459386,
            0.8433198877908112,
            0.5998435267670393,
            0.9331163579073388,
            0.7473276773883499,
            0.914652860213476,
            0.5399820119328973,
            0.7255849342393966,
            0.24515202079667575,
            0.7240060457690117,
            0.6797790363172005,
            0.9636781230274964,
            0.9621076396127072,
            0.526453558327734,
            0.20461044049262478,
            0.221737143318534,
            0.3042132609158593,
            0.7166006793681836,
            0.7208533268745831,
            0.14111583611047418,
            0.16505397977974479,
            0.6930709356098063,
            0.3469557583864098,
            0.4028006795191459,
            0.3031107087201411,
            0.39025588145942935,
            0.23485551066905996,
            0.5449188856687478,
            0.8810702418577913,
            0.0650207940766595,
            0.12237389969036438,
            0.7730308336766517,
            0.8004635175819177,
            0.6048075860557162,
            0.06523207730761515,
            0.40485317304823243,
            0.08902021468999932,
            0.20730816080223702,
            0.21001234293356918,
            0.23199645465127916,
            0.8123002420509785,
            0.3499116098336068,
            0.7499122414990476,
            0.7866312792602179,
            0.12174694137861608,
            0.32405932418634065,
            0.8258813665400088,
            0.04656678385188917,
            0.34120591602043515,
            0.025432139703123524,
            0.7513611942051909,
            0.40921909742638385,
            0.8768715967745427,
            0.6233021962940871,
            0.321448374528979,
            0.22230905602996653,
            0.6042217477703352,
            0.22034890014792163,
            0.9012174395684431,
            0.3780544593364079,
            0.17182226098290831,
            0.9536597136299334,
            0.7013268968862395,
            0.7957401114548772,
            0.37762917847620403,
            0.6641353472252048,
            0.5990337788510948,
            0.8244425305058642,
            0.6336964023073468,
            0.9442533024008415,
            0.9856053064515053,
            0.23764177109429674,
            0.5153091466750802,
            0.22192209755800385,
            0.653677202419529,
            0.08566875119109962,
            0.25922411512320986,
            0.7756516972996109,
            0.6518894605239182,
            0.8983646890758403,
            0.7839468017430613,
            0.2928502179805963,
            0.858351123436328,
            0.41833268377774047,
            0.2314661073036962,
            0.759291174791694,
            0.2680111781356541,
            0.3488173733450649,
            0.6756344422792672,
            0.7115499536533283,
            0.43697837993273214,
            0.2296222477305303,
            0.9561418763126147,
            0.2360168383852771,
            0.5707091940070331,
            0.1223442272970009,
            0.7920142561811463,
            0.11604585925925615,
            0.9083026514927254,
            0.340155551667485,
            0.6112643982241555,
            0.2015453241965941,
            0.49085157267329715,
            0.24211104326232125,
            0.45282116527917005,
            0.7562240898603866,
            0.7309128543377432,
            0.07706898694016251,
            0.6087936043086346,
            0.10628692407945894,
            0.9562681558712472,
            0.1307744960951679,
            0.4684732006832625,
            0.31153597327978666,
            0.6428299893138236,
            0.3865890604057184,
            0.7005259891521617,
            0.5180895174480458,
            0.8733436608237053,
            0.11341584270240046,
            0.811670609222333,
            0.5948402781373923,
            0.6543611792928933,
            0.8203804051476661,
            0.9439613667301399,
            0.04885500824368505,
            0.2639193725053357,
            0.39181319459953656,
            0.6963701495177078,
            0.7984729628367462,
            0.08962455833811778,
            0.20513997094453396,
            0.8229728748865878,
            0.6826715373558596,
            0.8869606824313254,
            0.32075328872089903,
            0.2715256841091682,
            0.34871567943039405,
            0.5057068180254091,
            0.7108863566584402,
            0.017315503417683065,
            0.11129374732080222,
            0.2149408872089178,
            0.03972142104021714,
            0.7114201947815033,
            0.7735142558448584,
            0.08096205710211934,
            0.7264487468336878,
            0.7275229961863563,
            0.1701241597350034,
            0.4554828558908014,
            0.3674820156462085,
            0.10296088297118644,
            0.907807960856776,
            0.9315090425783622,
            0.02269088145021081,
            0.8768534746621365,
            0.5143117619574731,
            0.7623025158310304,
            0.8165144399320536,
            0.9478885317309997,
            0.776460718598049,
            0.2818285909214623,
            0.5313107639119145,
            0.5223934559866608,
            0.5908742994286429,
            0.15799529054304107,
            0.6957020996541937,
            0.14067939009805353,
            0.17976054718260315,
            0.8553325397169877,
            0.7751947200973635,
            0.6542489234703435,
            0.9359516544956115,
            0.18910644338747085,
            0.3060744915742655,
            0.07156522488893935,
            0.7858905172353533,
            0.30841426063537336,
            0.7961153602347614,
            0.5242480442051327,
            0.7608381696457592,
            0.7321043646809537,
            0.3160923005617713,
            0.6308133085131272,
            0.6404187406431489,
            0.0314570800078281,
            0.6381340941591219,
            0.28335780818324463,
            0.8080440852641214,
            0.6650322810432938,
            0.7382350607745667,
            0.9417050501423426,
            0.4616591796097078,
            0.35170317130919027,
            0.735261952464759,
            0.941824765484256,
            0.0704397787913904,
            0.8025735792631282,
            0.884345853448629,
            0.36856840140455305,
            0.5384999491079693,
            0.5088196392361701,
            0.3690437535318991,
            0.6893065321084926,
            0.4043065001820547,
            0.7321490028319614,
            0.9953331565039383,
            0.8708898957578411,
            0.5678444419468969,
            0.6948121027529817,
            0.13237429831154945,
            0.758791690392103,
            0.9000474656571474,
            0.4130616897175554,
            0.6048306050375922,
            0.690857261930525,
            0.17608744899984585,
            0.8144564217524697,
            0.008916716494768329,
            0.3728910723008364,
            0.14230574725308864,
            0.07032084742055866,
            0.13510122662840574,
            0.7110610697088638,
            0.3230680758079223,
            0.8971823950988784,
            0.7593906944269069,
            0.7180945813078534,
            0.40156447934212547,
            0.31446765260880194,
            0.590384621574585,
            0.45397604575440187,
            0.959643273040757,
            0.3643877880513411,
            0.43129660288653504,
            0.8882110225380889,
            0.721800188113398,
            0.3597821910241765,
            0.9117927009769277,
            0.3174172692380255,
            0.7337327946421679,
            0.4101950457979521,
            0.3966570730442849,
            0.9368223197108299,
            0.3461667670681947,
            0.5215603324247206,
            0.11744811566395297,
            0.636498465619982,
            0.2038693777421673,
            0.9303912197571679,
            0.816998163909247,
            0.9789272639916347,
            0.6892868829686003,
            0.6982463926962832,
            0.841847840269864,
            0.25713058055406,
            0.06088862527493233,
            0.245800983643233,
            0.5342357284707632,
            0.9952357763786429,
            0.2441059118185811,
            0.8036209652416689,
            0.687229655807877,
            0.8475116280256628,
            0.36684229689996317,
            0.42475107653902533,
            0.9140454655574699,
            0.2679255563124284,
            0.534492910235425,
            0.1461389965077352,
            0.6985612438516512,
            0.8781293966055371,
            0.8839097809041689,
            0.43478503161232085,
            0.496953743989921,
            0.9862174771277655,
            0.04159572606291473,
            0.07255688902459867,
            0.9498054639038871,
            0.570391827242604,
            0.49001305411912444,
            0.8530655079238382,
            0.7419750948446635,
            0.49794852153088176,
            0.3289905594740178,
            0.5264460549027793,
            0.35096788116253996,
            0.38039435732459825,
            0.7528001382362867,
            0.7955875745990474,
            0.8484075180960026,
            0.5718070080412917,
            0.664181837639004,
            0.5930016381927362,
            0.7722245984189531,
            0.3454918499428087,
            0.598053446867222,
            0.9121501991469333,
            0.507109495465786,
            0.5620786175441324,
            0.8731547356388709,
            0.6666694634544433,
            0.5270620554009692,
            0.21624156898941072,
            0.44549942626809236,
            0.9034774952953463,
            0.6282940981219879,
            0.10859408808142523,
            0.07679682374546248,
            0.7015771280590647,
            0.4940827467634742,
            0.6153747570296487,
            0.1897752420621278,
            0.7028657997775055,
            0.5448505061986961,
            0.9921027559867645,
            0.41950910409868847,
            0.13178954908140816,
            0.2383518904066938,
            0.7815615667973201,
            0.16895292701555942,
            0.23169578068378394,
            0.3124439808679126,
            0.6233459461330343,
            0.4349122823170706,
            0.39269732832045945,
            0.08541285410083688,
            0.18466437935111302,
            0.719077270753475,
            0.7289340738984056,
            0.5193818534486477,
            0.354789781075149,
            0.6030125564046028,
            0.9734844474127914,
            0.5788586904350241,
            0.8896532902937736,
            0.3971867904276313,
            0.8112369010323465,
            0.48111285523424907,
            0.14542610042311843,
            0.9279678895807261,
            0.5356954396762273,
            0.1852933106306257,
            0.3561778722680313,
            0.31317097177836395,
            0.3551128084171603,
            0.08163166610234718,
            0.3156656187869643,
            0.9227454628213471,
            0.8362487440447994,
            0.39868405125484874,
            0.25902201552505455,
            0.2332786851666846,
            0.13660977228406157,
            0.06455443947244532,
            0.8168118066925686,
            0.7645494702824027,
            0.34009410584127797,
            0.47355152908799425,
            0.6852613137806506,
            0.368704208265222,
            0.9205666526517542,
            0.7954839177184126,
            0.9238677728627528,
            0.5541829847437505,
            0.8340909169505104,
            0.44100901165142437,
            0.19118634047301042,
            0.5663865981345195,
            0.3949474219220366,
            0.6983211472251544,
            0.51050268058169,
            0.554548371059581,
            0.28516342269217076,
            0.3959973967500061,
            0.09362588841780872,
            0.14196882945885458,
            0.2590132572403807,
            0.21249760169673348,
            0.24510066104064332,
            0.8808342100052384,
            0.5885744306658329,
            0.6046618092366636,
            0.8049617843116875,
            0.7213516051413413,
            0.16189442021410227,
            0.13991712115866184,
            0.6847675452857435,
            0.5968706224800094,
            0.6782643285889133,
            0.6898580897254263,
            0.5662067190549015,
            0.9583244345802936,
            0.2912642684755796,
            0.7630161101088999,
            0.03281787135301295,
            0.6971232011889607,
            0.4513833334496118,
            0.7160717719914459,
            0.6744556167554911,
            0.7012838246074843,
            0.09747407973728395,
            0.39103018910292175,
            0.6401692990540451,
            0.8732274653813095,
            0.1990875759939147,
            0.992779916472509,
            0.17438406058704536,
            0.8965750031118914,
            0.5548778214789011,
            0.09041896734415455,
            0.8202251402395749,
            0.9217434077259461,
            0.12848714981921638,
            0.4423636010983777,
            0.6245854605562289,
            0.8479875577813115,
            0.21833062484336352,
            0.08978365634540908,
            0.3971703136588606,
            0.270921228050496,
            0.2082154959111303,
            0.30839569718340565,
            0.7813961974128387,
            0.4284726179385906,
            0.11102333241822904,
            0.9916589209720094,
            0.9423505009705155,
            0.659033078754542,
            0.11169527470064744,
            0.8508594981950214,
            0.7756263095253668,
            0.01958367233718139,
            0.6671403237915123,
            0.9931912176940958,
            0.1828227445986681,
            0.671083610894634,
            0.3120957376876963,
            0.05823283339664698,
            0.4539694522719664,
            0.3827425186513863,
            0.8161555016670786,
            0.6909702044024906,
            0.5697887820353031,
            0.47684073212447675,
            0.1174826727347773,
            0.9199399451192102,
            0.5341426981168305,
            0.3843053536441867,
            0.28744865946458,
            0.847420343974517,
            0.2090150894636853,
            0.3749965328313567,
            0.8057213252426457,
            0.5005627406838957,
            0.5143571962319217,
            0.7657339905459002,
            0.7329828664873212,
            0.2440568382361218,
            0.8502214304598523,
            0.6260033891681143,
            0.06626999120022115,
            0.058801167239939334,
            0.25972930664267024,
            0.6451029221057365,
            0.6260012237257966,
            0.5846441474757913,
            0.98815366379417,
            0.05783009574274389,
            0.022349918869454588,
            0.1600834646879955,
            0.08953514758686276,
            0.8732084555182431,
            0.8115969955208467,
            0.23373497446989266,
            0.10840903372449262,
            0.3171564540123466,
            0.6658826853300668,
            0.43676485701893675,
            0.4344333657895355,
            0.07897530154578136,
            0.016469919896786256,
            0.5675235011316633,
            0.5808433804516457,
            0.867716441356151,
            0.2065739667627342,
            0.25440188873038416,
            0.3422924158271836,
            0.3953463754311852,
            0.5749685502945107,
            0.1488782862289172,
            0.7394424780124986,
            0.3390228551044585,
            0.28553557726203915,
            0.8153568215504137,
            0.024964426512648186,
            0.10278618583088106,
            0.8669568659754601,
            0.7881946366992756,
            0.658616381755007,
            0.11159208601537995,
            0.10096754637870087,
            0.6410275673490674,
            0.5332073715199348,
            0.8536682385385184,
            0.3629141869892666,
            0.4737316407585195,
            0.33920893131595764,
            0.5363296458774595,
            0.7991161654752915,
            0.8448716812778778,
            0.9529552780758653,
            0.3430740253249046,
            0.5199790758371671,
            0.5085226165222254,
            0.9476254029397022,
            0.3888290071361509,
            0.8298957295600268,
            0.015354702895921113,
            0.026795420102779577,
            0.5623317424080893,
            0.44155098929951186,
            0.0216140477018405,
            0.9153672471307774,
            0.7322390118784943,
            0.35474773974775453,
            0.8456112753342347,
            0.12427812853910869,
            0.8178060502734751,
            0.09769257732793735,
            0.2352335200687209,
            0.5219444775119129,
            0.18953776590190818,
            0.7089965698272449,
            0.9271773068939287,
            0.26661820468540565,
        ];
        let subdiag = vec_static![
            0.37671885831556107,
            0.5914902345047548,
            0.4573673949813909,
            0.278061312576772,
            0.45512214158576403,
            0.3112899705911143,
            0.4880214441872541,
            0.9582807681021962,
            0.48963048000783027,
            0.3250457425435612,
            0.07352275648645279,
            0.67413401097283,
            0.3100466148803773,
            0.562276992540448,
            0.3892394671456678,
            0.749936908252996,
            0.3694048046552584,
            0.7083582626246234,
            0.6251830717953774,
            0.7884290671362195,
            0.08925884337373147,
            0.2613537478548059,
            0.46587313139923503,
            0.701485596521086,
            0.33649429784277063,
            0.6911793664914623,
            0.9948554052777019,
            0.17308968749086218,
            0.14674255849462148,
            0.8732244248197987,
            0.9456441706261545,
            0.640728931177753,
            0.7986202000553076,
            0.7023010978504587,
            0.11704483536140065,
            0.884159157005428,
            0.38767049994077984,
            0.33752430428623426,
            0.3799123019580549,
            0.00314019945840871,
            0.6190706208888044,
            0.5102826797204365,
            0.6797651848612186,
            0.9399355917564993,
            0.5581123097220246,
            0.12682711067698937,
            0.6538602970641864,
            0.9045235067178719,
            0.6892314551830656,
            0.9104493575898686,
            0.3174388259005446,
            0.07255503694667453,
            0.8195907836202596,
            0.2065308345615483,
            0.6563964950866789,
            0.8285736443349354,
            0.2807382142533439,
            0.888192042410335,
            0.7281834856246764,
            0.4213180062854599,
            0.3891228421856179,
            0.9700914470984294,
            0.45160015526230735,
            0.581090670815197,
            0.5935074179617855,
            0.7314200085933598,
            0.7668608047743345,
            0.9645838183585224,
            0.5595954893259129,
            0.501403728329053,
            0.7252189956851528,
            0.3877102331948553,
            0.7312171132795519,
            0.45921662392553675,
            0.03569997105246314,
            0.11035683929716755,
            0.23537156497955436,
            0.7453083988700915,
            0.16039524701959074,
            0.38914641522433013,
            0.6488288881008611,
            0.7802459577505126,
            0.7855394167175755,
            0.9525845711062543,
            0.3835654723986076,
            0.6540539835774286,
            0.5411639500064723,
            0.4824262094927344,
            0.9023430874410875,
            0.559384552682,
            0.5198552240452828,
            0.08131268459802798,
            0.9648278171342273,
            0.18547718903819543,
            0.018187371199786972,
            0.3548694940703081,
            0.5769353673932345,
            0.8277144778427715,
            0.41156560741241444,
            0.30580641025730293,
            0.4994004898094244,
            0.6543879049826437,
            0.03768458013740572,
            0.8847452861945968,
            0.17246126984418086,
            0.8021618206146044,
            0.6792215520070621,
            0.1494249976088271,
            0.04323242654960879,
            0.46497519643274443,
            0.7311303757347277,
            0.6907293989179726,
            0.5561071159235972,
            0.8664847074180113,
            0.10656582443043194,
            0.8943531293401422,
            0.5810938327611845,
            0.4079086398883156,
            0.4376953742835751,
            0.5150452019463021,
            0.4654673204490244,
            0.6181269596797001,
            0.4972856911548612,
            0.8791344234134285,
            0.48448853677476866,
            0.5271322135896357,
            0.9187839153765355,
            0.6432254405537613,
            0.1395032787202216,
            0.4617483512605155,
            0.7439775577234952,
            0.20688487763651586,
            0.6837970698781676,
            0.6284010284439561,
            0.9877247981721398,
            0.2593161005225657,
            0.523627762922784,
            0.22137969255505463,
            0.7131606420460196,
            0.2852462931306611,
            0.5124483971307822,
            0.9367014111119283,
            0.32731755693626097,
            0.964236792324476,
            0.7493694578953238,
            0.07744815323017573,
            0.12949889017954386,
            0.2641939259367596,
            0.20324028762179547,
            0.713464930463833,
            0.7854027536448535,
            0.6805436303675616,
            0.8942998010145956,
            0.6086916036361911,
            0.67377363814928,
            0.6969042644434692,
            0.45367268191922705,
            0.02611844492272608,
            0.4700011168748164,
            0.006154223920945445,
            0.49785132681634103,
            0.9801180571483392,
            0.23142606315597036,
            0.6857808368058479,
            0.13658722793896216,
            0.2253508839222368,
            0.24281067691841418,
            0.8526910646423107,
            0.4902538927391915,
            0.22738048994855875,
            0.7705487908544527,
            0.6240777789061516,
            0.2755720074547112,
            0.9203432076130207,
            0.5971832484284051,
            0.5435267728965103,
            0.6279113755719903,
            0.32112529775845144,
            0.22720242153795212,
            0.6868459996576608,
            0.3458489741208203,
            0.4386189341171709,
            0.5662864682421069,
            0.0915222517019979,
            0.9327725500558302,
            0.8388716532891217,
            0.8354865519804079,
            0.6436987359219395,
            0.9536675701446755,
            0.5023348488433156,
            0.03744165598145255,
            0.691465268394428,
            0.3471961034273211,
            0.9487485590405136,
            0.08059004207151443,
            0.9130473718156124,
            0.8711760574531869,
            0.8984710118710725,
            0.38433446373471525,
            0.6047403061075196,
            0.40230752044977547,
            0.8999549061609351,
            0.3045400555681539,
            0.18733188204216666,
            0.47881738310505884,
            0.09948824662632694,
            0.8107201856317277,
            0.15303290808660974,
            0.09469168130866801,
            0.6229102667450735,
            0.17724775681237315,
            0.39906288328249606,
            0.949347149715389,
            0.43036938994682195,
            0.0026488356345022446,
            0.8051683567235493,
            0.7142222915323341,
            0.6565513212405722,
            0.8186110241383198,
            0.1075867852301704,
            0.00651363620150236,
            0.381315358116759,
            0.8340802896496835,
            0.20799848101982932,
            0.8380203793470061,
            0.551697397258092,
            0.9741153378014853,
            0.6716747126650318,
            0.5405891135029178,
            0.6401015262720249,
            0.960909912158254,
            0.7755110312141643,
            0.825478905278364,
            0.5064634220126577,
            0.18812888182280796,
            0.07283708755624119,
            0.8220237358112901,
            0.47238295701015254,
            0.6869923481614207,
            0.9163118520579482,
            0.22074610201161338,
            0.9919447342875314,
            0.6222215472277868,
            0.05033016452819428,
            0.9254287027213731,
            0.24236628317521214,
            0.2968826172357997,
            0.1346880107440669,
            0.1049489403202527,
            0.46459571678534284,
            0.24948187054913806,
            0.41911978648494297,
            0.09618836090256044,
            0.05268542651235997,
            0.3316174971449575,
            0.04388767991512044,
            0.9807739468209036,
            0.5532017827965474,
            0.5092525272651334,
            0.8587394080322006,
            0.23648035753216734,
            0.17429808586148532,
            0.5447600177654469,
            0.1476302487546165,
            0.904924948067489,
            0.8467065400997184,
            0.5588587037416475,
            0.46532532965862294,
            0.4149944587994253,
            0.23699178409866384,
            0.8286742504857455,
            0.14964344260285367,
            0.9419164055233096,
            0.9931685556787592,
            0.4726614335735072,
            0.946096426095138,
            0.9665484700176549,
            0.07045472429632438,
            0.7287290285529795,
            0.8288758892449009,
            0.7345986156299534,
            0.47027165222078315,
            0.7751737277463969,
            0.9166110660109572,
            0.6205218974962594,
            0.3932245033802777,
            0.8655119836283389,
            0.8156558903134565,
            0.8276658886810031,
            0.2769252733692703,
            0.36745540867630144,
            0.26391095661736186,
            0.3870335115679614,
            0.18833085664599503,
            0.015739548377707413,
            0.6947652147498935,
            0.2621931039418285,
            0.4154570640135682,
            0.18354694016920148,
            0.6002442189687933,
            0.21341866623144667,
            0.014316938347873842,
            0.08793627028714246,
            0.3392283957313005,
            0.13925243425867062,
            0.8548106737856321,
            0.023344909551561766,
            0.3025207610425611,
            0.945332983928621,
            0.7773281993770145,
            0.15438666764019215,
            0.5686998114809476,
            0.425263658454611,
            0.5698154725148552,
            0.31995833736537793,
            0.5130449716086514,
            0.5415413773729916,
            0.5070504977672251,
            0.7314785852161422,
            0.9164413704713618,
            0.47868307722193637,
            0.2797797423204419,
            0.36067713495419584,
            0.31363319118611555,
            0.584081118667626,
            0.9605866755599066,
            0.029448776145766686,
            0.010061835212571646,
            0.22825920384517806,
            0.4407086297444909,
            0.10409337117967787,
            0.4281760291107445,
            0.5764734569225264,
            0.499293343590413,
            0.01156525855674373,
            0.6298199613506629,
            0.49898816079666986,
            0.3276462012959164,
            0.10406410400424715,
            0.21298925796192814,
            0.7143447965846307,
            0.14330778019390145,
            0.5220594982174991,
            0.1612667007268822,
            0.6588380169227596,
            0.4461368189323778,
            0.7678796507689396,
            0.666856979318627,
            0.014515023557269857,
            0.06127676491411305,
            0.597597408831207,
            0.37768829596205644,
            0.6219969152775721,
            0.9583181166230411,
            0.6332556393902555,
            0.4259492962772,
            0.07414642967222973,
            0.3986185525248154,
            0.9951481013998621,
            0.22221987650085284,
            0.25699804840117135,
            0.6179696994779834,
            0.8873487040207438,
            0.22843263145778092,
            0.6136308661271643,
            0.20955478035522423,
            0.3625123515888926,
            0.24938304785805532,
            0.8221863612689626,
            0.5369326805849128,
            0.3074126328324287,
            0.5063245676951496,
            0.4159538708900693,
            0.9826556534402682,
            0.4998560106432337,
            0.6554358467129536,
            0.2983117434500435,
            0.8151056082766394,
            0.5617812186813114,
            0.6034086492730604,
            0.7672580488648616,
            0.8394903728876504,
            0.8952815873470424,
            0.20341397101519865,
            0.806254091478268,
            0.34267625270062285,
            0.9476407294662152,
            0.2064797236007565,
            0.7119644394035383,
            0.5827323992955744,
            0.6333999682656258,
            0.6269038390180789,
            0.7306530101971123,
            0.5882072413692148,
            0.37411987116797396,
            0.7394626084285107,
            0.6034979112638256,
            0.9497021206516011,
            0.5288288204083893,
            0.2145851268473622,
            0.4127087781332467,
            0.7919434793274045,
            0.032275117628955186,
            0.7981343755742937,
            0.4236652909693067,
            0.6346653999042923,
            0.22605950058719504,
            0.23353327390338197,
            0.6454368269955231,
            0.6903283666721761,
            0.050371688763550804,
            0.3029110335301539,
            0.7411095709216069,
            0.646547812318539,
            0.9947965405449165,
            0.2069106723312889,
            0.37728669576698826,
            0.6167675225215548,
            0.00037982409956549557,
            0.13918512269609817,
            0.657012324268671,
            0.6996147873421552,
            0.9884209510126968,
            0.10207112600773072,
            0.019589969842475186,
            0.336964372718644,
            0.649511178652959,
            0.873933177063334,
            0.8072005961299136,
            0.24126519965699433,
            0.08087090399306507,
            0.3117059701347713,
            0.08713456880487291,
            0.9112616635359201,
            0.7724985234587572,
            0.1420125872236404,
            0.4067531007183758,
            0.4565126887800255,
            0.9900905198977834,
            0.9539849962805971,
            0.30592877425803555,
            0.37379672981349643,
            0.14814228940159513,
            0.30108786205609817,
            0.28475225344120836,
            0.6949194401093525,
            0.06133195651122192,
            0.1266008554617476,
            0.4678260487872101,
            0.13067096758710472,
            0.9966025134228441,
            0.8412390691232315,
            0.33677990591679996,
            0.3202667802084005,
            0.822367534698531,
            0.7484547922817696,
            0.580971290578795,
            0.731768755265186,
            0.656774887520502,
            0.7562236849426837,
            0.7324615664579461,
            0.02605317808061014,
            0.6252775532655153,
            0.4880586770226836,
            0.15830320642227313,
            0.1577722612303587,
            0.9399377238705633,
            0.09199236365438479,
            0.5141857949052452,
            0.20520174663841795,
            0.7312832640957163,
            0.8512997175885758,
            0.2429844189801832,
            0.6962563154025011,
            0.2911701833779504,
            0.39475481571321036,
            0.7029050839702605,
            0.6484825002668674,
            0.84235527303145,
            0.6778076937926241,
            0.6545263168073979,
            0.5140095648203321,
            0.26849315235961557,
            0.39907949272234455,
            0.98699167127683,
            0.21175821385418037,
            0.2727480230175171,
            0.9242951744683571,
            0.9544291666832672,
            0.3799670059501187,
            0.6075396673376375,
            0.3689661320555503,
            0.533480654790047,
            0.3948759679148285,
            0.9792483973292584,
            0.4545600614965364,
            0.25104389201596145,
            0.3016669883944194,
            0.5778596873800093,
            0.8821438126300893,
            0.2394120582161572,
            0.8514555613182432,
            0.02728197763223017,
            0.95618909135417,
            0.988317490479576,
            0.4656200643386421,
            0.9164866947655901,
            0.2425520118547264,
            0.2786013763922671,
            0.6409225165618465,
            0.4322896551380161,
            0.896283621442633,
            0.3528279710987984,
            0.3458776140371883,
            0.3926615598298674,
            0.6073580809091094,
            0.8978424554512642,
            0.6136497663097625,
            0.8015478709326808,
            0.3000791080727212,
            0.2612365927588616,
            0.49266658420487863,
            0.6204823307612688,
            0.7081517751636764,
            0.9873668343127493,
            0.7974268715138637,
            0.8552507847007327,
            0.6241304039507437,
            0.4493355957117916,
            0.4997017785448291,
            0.5423048132829118,
            0.7977727346207493,
            0.9471571348619552,
            0.10614004964510082,
            0.4668591435509476,
            0.06427964817407728,
            0.17870813686089104,
            0.40245064946196807,
            0.7014738544339131,
            0.5898819217035093,
            0.4060535075147119,
            0.5965737853917953,
            0.40369116418676576,
            0.8402342577916865,
            0.5023421457456148,
            0.5592856881877423,
            0.9876780795149107,
            0.38095436627099755,
            0.5418480772861584,
            0.5201475180818953,
            0.11077651762226226,
            0.04891875740038698,
            0.4321120103913775,
            0.3313372742768437,
            0.9524091629081568,
            0.7351481295656412,
            0.9802285170280726,
            0.03256205399455114,
            0.29622581219576605,
            0.7504473724310984,
            0.023610198380436653,
            0.35225229074093023,
            0.20623013094606446,
            0.18395329863493248,
            0.21162417115461296,
            0.266258371833647,
            0.7910894135194491,
            0.2948732772325322,
            0.45193405824630484,
            0.7827640072063997,
            0.3272464693835584,
            0.3590703348938932,
            0.9053887182392713,
            0.260734491433541,
            0.9335424084074295,
            0.4895282182065558,
            0.6093493355254171,
            0.827229023701996,
            0.5260395735713417,
            0.7530094768179988,
            0.8344653425327245,
            0.0175433083881974,
            0.47663052519822213,
            0.49355625329240416,
            0.6760088943526494,
            0.9183319182023756,
            0.48031034432991215,
            0.033144339236412024,
            0.9579611864275008,
            0.9355921241628944,
            0.6100038886322742,
            0.059582063986656,
            0.9304901072202086,
            0.6490490199389266,
            0.8133501581536393,
            0.5443970163208959,
            0.7895508898069663,
            0.008234208936151788,
            0.19448701666375245,
            0.5857552231262603,
            0.13161503863434043,
            0.8430986706515197,
            0.3595573006222008,
            0.7965981295941231,
            0.7622607721891685,
            0.8564553956523567,
            0.590949180036414,
            0.08410767390646223,
            0.3392872183125214,
            0.9667007736840229,
            0.6828970072620655,
            0.10869524768152206,
            0.9054388671728684,
            0.2510120984459946,
            0.72683154583824,
            0.9065014251550254,
            0.8240588017372226,
            0.39832881136411735,
            0.29586566076513443,
            0.27231653061202776,
            0.2225041595115479,
            0.7531107061947561,
            0.13623618600686382,
            0.962132738206387,
            0.09202790330977118,
            0.48912293025621323,
            0.4662671532771704,
            0.5724524802462818,
            0.10862916922174215,
            0.5097504645705111,
            0.4734157974868579,
            0.5936398000748225,
            0.5706760165639232,
            0.47877146129242354,
            0.3569484728678505,
            0.43750524080573117,
            0.22836690846107333,
            0.05014670163853774,
            0.9342739907641169,
            0.9123235129953162,
            0.7393837398896341,
            0.9614384354013058,
            0.2632619151416744,
            0.6015720930528762,
            0.2915538253232436,
            0.39651861916564457,
            0.7251375049603032,
            0.05867306112415971,
            0.16352602214241463,
            0.9770475402469034,
            0.5284184240147222,
            0.5762730072134777,
            0.4495121401495401,
            0.9765338268274393,
            0.365207870200173,
            0.12708090128554583,
            0.6027956048763583,
            0.5454357893582741,
            0.7111229935420789,
            0.9453585140521719,
            0.6393982206310176,
            0.0044447432736431924,
            0.39558040065831745,
            0.022294500695522745,
            0.7880293225490371,
            0.035977054535484165,
            0.4356643142262643,
            0.34794382350216524,
            0.4418759628658767,
            0.7548667280976449,
            0.2985768707213895,
            0.7887556919107688,
            0.024581552417021757,
            0.11479114310376104,
            0.6744517238457035,
            0.21380811554405432,
            0.7801353746945496,
            0.8945839576525288,
            0.02758538453796866,
            0.3035020839379008,
            0.9411808986768297,
            0.51154150401942,
            0.05196362821474476,
            0.7564913935174579,
            0.4945397921866519,
            0.07571769465829692,
            0.47176023514193377,
            0.9412222172978777,
            0.17821558159513373,
            0.970098583312266,
            0.049678001561435536,
            0.5282543131824963,
            0.9195636690845274,
            0.580707894992502,
            0.6436419240880916,
            0.8828698558116453,
            0.33456339231155086,
            0.7130944538737045,
            0.381334999001282,
            0.5283525925748707,
            0.05570331945995366,
            0.4275057645580982,
            0.10356525324389354,
            0.3448219306673921,
            0.7507575543143622,
            0.615387395817341,
            0.9538403086071486,
            0.49489369103001113,
            0.9342638137384386,
            0.9256850304480524,
            0.5880487343901908,
            0.17958373014335627,
            0.4091517089852903,
            0.385026519006998,
            0.45249038383133466,
            0.9858503372857962,
            0.00031198828893852504,
            0.3719068262043139,
            0.10411368790662046,
            0.44320908869970854,
            0.8951858582652665,
            0.19294289592225955,
            0.5232524924984412,
            0.6001668435571029,
            0.19734738436204435,
            0.37626328940149556,
            0.2897811525513808,
            0.6364073350780381,
            0.661498148086808,
            0.7813931421859311,
            0.6278265170462115,
            0.7583740975472366,
            0.6799175342481804,
            0.9066999579443696,
            0.3475828478605564,
            0.060343370769859406,
            0.202507435661069,
            0.8813568791886893,
            0.06942037583836713,
            0.3318909739561675,
            0.38693708277716576,
            0.3147812838735806,
            0.9083694614390199,
            0.163219723845336,
            0.7627845184211082,
            0.3966225506877895,
            0.3025087400461888,
            0.827305122508102,
            0.15974159228331752,
            0.5017386939774493,
            0.019212456406948353,
            0.1624888049671609,
            0.645504196486869,
            0.38307083501726735,
            0.40711015264386474,
            0.4528963731958805,
            0.11597437855385284,
            0.6381777994910081,
            0.3906872686622783,
            0.26976485437222464,
            0.26994263150216347,
            0.09705594563167386,
            0.4788574085815859,
            0.9775695003785172,
            0.6949308334368314,
            0.3463339983995992,
            0.6085143645378281,
            0.6286543736685208,
            0.6704200601265262,
            0.6444867982337624,
            0.7525131003940668,
            0.6670108559661567,
            0.3424460557095089,
            0.3631098600861672,
            0.060289091839881515,
            0.29590063473634287,
            0.6525899475679741,
            0.004016733044088228,
            0.6829409304109124,
            0.4207109732553651,
            0.9983817489619621,
            0.33796620630760343,
            0.21825058956525878,
            0.7145208077354601,
            0.3730746011421181,
            0.5148923528528624,
            0.359517615213803,
            0.5388128303438028,
            0.24612964336995868,
            0.8348211740910664,
            0.3680799354334652,
            0.5938777901835578,
            0.9693619278997123,
            0.63502699914433,
            0.2893253960008174,
            0.13509250325327038,
            0.36862188742042024,
            0.29663855954539364,
            0.014634597819981887,
            0.9838685067729482,
            0.40534103683651823,
            0.7403701388181022,
            0.16835583265547993,
            0.12700460220642362,
            0.7207691595571962,
            0.5895587933186803,
            0.13801375702961016,
            0.7365407266091945,
            0.895877103522504,
            0.8181127596953975,
            0.5512076632229491,
            0.43804704130036365,
            0.3047890197125347,
            0.05871850867556605,
            0.5664267566275901,
            0.6757612704343661,
            0.48814173731731725,
            0.9122164606471833,
            0.3953974538772719,
            0.8540596204791605,
            0.6624972009780908,
            0.1368244448223419,
            0.9002879813217955,
            0.03367267607122437,
            0.5859187152901306,
            0.5939521498370595,
            0.27070049883692604,
            0.3874703426725369,
            0.6316320569590927,
            0.2753649119788282,
            0.6983400164467722,
            0.436767193768297,
            0.7069482709119186,
            0.8570946674945635,
            0.2553948436480411,
            0.6216992746613719,
            0.6004845327072269,
            0.8422491143872637,
            0.9498636512812554,
            0.3866497784950864,
            0.28374422577522496,
            0.4484194706153507,
            0.15600873280876326,
            0.4110822407480742,
            0.6634369279220605,
            0.3320855993415712,
            0.1311963787482282,
            0.1016464738803563,
            0.6595722102106649,
            0.38410774470653086,
            0.9599050501610701,
            0.7171562328698321,
            0.5062967661500575,
            0.7461308001849676,
            0.4770988836615446,
            0.42913775832385126,
            0.1291400652996738,
            0.093764427689083,
            0.5267191753683139,
            0.49264539213347924,
            0.41107561151809036,
            0.022091368689010404,
            0.015325726632782755,
            0.8697551765369783,
            0.6837995372622451,
            0.7878106054084043,
            0.25831297384252294,
            0.8134685055868323,
            0.36202799069146974,
            0.9757682306946027,
            0.6473127267758995,
            0.2986449225504132,
            0.8881611353540096,
            0.19239744013030524,
            0.40733335986526575,
            0.5321704089004125,
            0.09898594195868682,
            0.9507441811197719,
            0.844880344564211,
            0.03731918415138358,
            0.3385622493858168,
            0.8504023048799197,
            0.6989467669923123,
            0.2549675349670987,
            0.48572089933797746,
            0.07288131474554993,
            0.09191220411594636,
            0.31033556092635084,
            0.30395736119114725,
            0.3846459319653658,
            0.392841391931931,
            0.24653075820003234,
            0.8544516886192999,
            0.8245326833288107,
            0.9958164191489396,
            0.19854830880772956,
            0.7766367108118657,
            0.09761322283985896,
            0.9417275796434791,
            0.7850343271348909,
            0.28609148479516555,
            0.5125689627715785,
            0.9258578542694729,
            0.275764740840005,
            0.6016164455627839,
            0.3462210019447792,
            0.3233452195422518,
            0.8409645559476775,
            0.7524922981717692,
            0.6339260467857786,
            0.9314935276749724,
            0.972677031160904,
            0.31299813790851694,
            0.8760082255657017,
            0.761333890179974,
            0.45938005370147283,
            0.520200318837958,
            0.6076441106575183,
            0.8724900021090676,
            0.8611036210839981,
            0.6552473684667536,
            0.6252833327380347,
            0.5185353691425693,
            0.9040227518479278,
            0.27970653915447574,
            0.9845390078155203,
            0.346673916502631,
            0.6623590611948782,
            0.006379473313236073,
            0.3734655087767689,
            0.7744014191561744,
            0.16961823466111714,
            0.3659953549414826,
            0.11118971782813591,
            0.759433495713853,
            0.1487394051434685,
            0.46865406255457365,
            0.8750582025359577,
            0.5073586413621533,
            0.41813721709898055,
            0.8391732370379675,
            0.5352984693712416,
            0.06008548992839924,
            0.350790921866179,
            0.21069799207917228,
            0.7949364947712025,
            0.39135641034895996,
            0.08709451091563836,
            0.8814193716323061,
            0.27314551566775214,
            0.9830695721780692,
            0.503725832798613,
            0.6084501117465266,
            0.47272365964120555,
            0.3892550561813739,
            0.09819293907928683,
            0.2384714035720421,
            0.08522948546776465,
            0.047686853879890134,
            0.5882039696597616,
            0.8672446036284184,
            0.27450605832508945,
            0.8613595906290894,
            0.8745126749842754,
            0.9851395552590442,
            0.4978119091583112,
            0.8993476815825009,
            0.7448498189517259,
            0.8369711602636397,
            0.8224174091515873,
            0.6822207719042813,
            0.1488219227059281,
            0.5778325840739624,
            0.4795873368483069,
            0.7090204945767113,
            0.15922992305806227,
            0.27438393598215605,
            0.2793210940950517,
            0.8884162176404946,
            0.6608015270385535,
            0.5282031642912364,
            0.9994618773829554,
            0.13146227355187012,
            0.14029156682207644,
            0.600131650519978,
            0.6390150442010555,
            0.31119136217286747,
            0.4016278174225866,
            0.07902734097141195,
            0.18629172890496581,
            0.8417267599628195,
            0.0009516721844141651,
            0.8728767092552249,
            0.5705298847912903,
            0.2329828050038638,
            0.1633535986935607,
            0.5443617553931892,
            0.9586315815409794,
            0.023573402817029132,
            0.630137650018288,
            0.3893839335340684,
            0.6032743106801581,
            0.6895868748541909,
            0.25749567302946685,
            0.9422588879367039,
            0.45962380488382426,
            0.13146747514986146,
            0.26518064000063035,
            0.7877056840494667,
            0.3289307264763762,
            0.6165600739898811,
            0.38624729302603034,
            0.8218929084592703,
            0.7273333375682015,
            0.6951739643262745,
            0.2620961872208437,
            0.867643751647387,
            0.5043461487128705,
            0.6162961579478152,
            0.9288355286850705,
            0.9298555904716704,
            0.019198104824075357,
            0.9225521021579641,
            0.0888584112444325,
        ];

        let n = diag.len();
        let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            compute_bidiag_real_svd(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                40,
                0,
                f64::EPSILON,
                f64::MIN_POSITIVE,
                Parallelism::None,
                make_stack!(bidiag_real_svd_req::<f64>(
                    n,
                    40,
                    true,
                    true,
                    Parallelism::None
                )),
            );
            Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n + 1 {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }

    #[test]
    fn test_svd_1024_3() {
        let diag = vec_static![
            0.060466268434667514,
            0.24070433133590274,
            0.05184276387772846,
            0.9898738802383618,
            0.5867224416658019,
            0.8920795530759005,
            0.8213260193280432,
            0.1274585126399569,
            0.5137105305003337,
            0.8929508711344447,
            0.34644482421179423,
            0.15662711364303783,
            0.13204459060563245,
            0.2452181097021333,
            0.5529961084728586,
            0.6059885561225518,
            0.335661034135773,
            0.8178650650229435,
            0.6928232210699408,
            0.6993333722077023,
            0.3614916242067844,
            0.7887570130392116,
            0.14888470133705733,
            0.15853694539661844,
            0.5334912050093811,
            0.3612993548593889,
            0.6903211472739849,
            0.64703468631002,
            0.829497094858506,
            0.2522343254330478,
            0.9505881136261928,
            0.7340919699859023,
            0.4445327799200357,
            0.276725070716636,
            0.8813837074592618,
            0.8641241736265327,
            0.37601439289318583,
            0.7406106431867137,
            0.8187897453440819,
            0.44679682056462267,
            0.6866279118940135,
            0.6322556970967156,
            0.5177506730663544,
            0.9643854839179787,
            0.6465100827097693,
            0.339197465636223,
            0.7513292599815653,
            0.5602603062963017,
            0.4553307839097991,
            0.22844009431280932,
            0.6235416261588994,
            0.2262804849429565,
            0.9136387602980167,
            0.9915699182157715,
            0.7259256237506346,
            0.3929852244527421,
            0.2155371379405563,
            0.8086877453574028,
            0.8647281751947866,
            0.606292073442245,
            0.30240091664534696,
            0.9225948840817058,
            0.6810625155003264,
            0.05179968431711934,
            0.595659576422282,
            0.7564014939381208,
            0.13794493869812707,
            0.36285564872228715,
            0.32342613546406007,
            0.79124383797685,
            0.03550783493526544,
            0.7487151700009632,
            0.6765223339310117,
            0.5937152610233598,
            0.07403103674795586,
            0.7606782796859048,
            0.18578166030690013,
            0.7013672177585895,
            0.2033016281334249,
            0.36779130656982795,
            0.6624345349315481,
            0.17276643968296868,
            0.7326612329736877,
            0.39683700964818025,
            0.9249585764714129,
            0.9058620912528339,
            0.5273041580770942,
            0.25056807692037486,
            0.48806466395096515,
            0.06552243597979834,
            0.978315141677103,
            0.502782492170536,
            0.5975218887295574,
            0.8954320387435921,
            0.18424721934266852,
            0.7682726736917449,
            0.43241327339139235,
            0.9531509198549333,
            0.9046304333439057,
            0.209761648896709,
            0.8410391805702537,
            0.10657531625818184,
            0.05568939898497982,
            0.676138899758238,
            0.532991981445195,
            0.6550486984863335,
            0.5855669735264619,
            0.7507383900993289,
            0.6547493864972989,
            0.39773180941485786,
            0.21977443508356898,
            0.20152541150538528,
            0.23513555154348653,
            0.6260455323467065,
            0.5686434999835956,
            0.3645862855059864,
            0.5982150616652503,
            0.681662158287625,
            0.5023884533718146,
            0.08596469818006636,
            0.865840915557725,
            0.5096733454968728,
            0.7376731774487919,
            0.23500068740937674,
            0.7760973488757089,
            0.30075646064672434,
            0.9565535059308128,
            0.144637054217305,
            0.766299241844306,
            0.9182047949601909,
            0.8981438083995072,
            0.10243727574939943,
            0.6444340540568536,
            0.39948153880812554,
            0.22358680321222313,
            0.5940835708596859,
            0.3193867223358725,
            0.8098588935943911,
            0.010859229221052202,
            0.41213053744762584,
            0.5781544958693299,
            0.2559845296086064,
            0.7173914473935831,
            0.3618426918318993,
            0.6545691051178975,
            0.16389232580120328,
            0.029853796333192628,
            0.1112042501011069,
            0.4131986127015722,
            0.0675709858058694,
            0.47697288079451994,
            0.7579635090222712,
            0.939295939451755,
            0.06104837646390948,
            0.6746647049785921,
            0.41976327511961575,
            0.9033609580534747,
            0.2726505261765354,
            0.6810347096753436,
            0.5761914873814167,
            0.5605593648243876,
            0.2040106732498883,
            0.06559590462849507,
            0.19867228693975358,
            0.3795569308165738,
            0.3576230887086853,
            0.6646129207450863,
            0.19159996290089576,
            0.26289150671048334,
            0.22984353830414195,
            0.10998864141249054,
            0.14726051758768155,
            0.09891300931363878,
            0.20699396396845948,
            0.4396333193710559,
            0.66049328467152,
            0.7484210613887542,
            0.6214537395445267,
            0.19715195461724988,
            0.26926297891853945,
            0.013177250804681129,
            0.5401145872074694,
            0.8876912160030009,
            0.3958473022669018,
            0.8525317391364969,
            0.9646251671547991,
            0.7639431892415399,
            0.6176857025453736,
            0.2440159000876102,
            0.4338943641942402,
            0.6325069093330767,
            0.42747587311352664,
            0.611567551714215,
            0.6500639751807277,
            0.837380450954655,
            0.2998521440987161,
            0.17958444155475328,
            0.3889472877193627,
            0.6898239835481556,
            0.8370090796660907,
            0.6064193851083068,
            0.4274634932199487,
            0.6087718098881021,
            0.508256409689673,
            0.339079489379435,
            0.10509571352293445,
            0.005572687883438454,
            0.021981179575465415,
            0.29212956768147014,
            0.7867865404724902,
            0.1807384308077843,
            0.8665190424853524,
            0.6184357021120035,
            0.7536704913156237,
            0.7271598615669912,
            0.49475392709260113,
            0.728471317065983,
            0.3269837398505393,
            0.4704197531972808,
            0.5738584097539486,
            0.758731532875281,
            0.7260845747787328,
            0.6016048024552125,
            0.8010556210240337,
            0.004072879110130434,
            0.8437986241962506,
            0.5146240323523598,
            0.13904073839194253,
            0.6089836301203322,
            0.897844926432131,
            0.49057508349361,
            0.29005256571070226,
            0.15772152541113738,
            0.4738990343662315,
            0.5021527238202769,
            0.6937852389489244,
            0.009814520346564715,
            0.15675068544014148,
            0.44385575330248883,
            0.4485313346936677,
            0.4217206515389108,
            0.7577444148847231,
            0.7258868622262982,
            0.6193382669120356,
            0.039861150798117695,
            0.4349519716620509,
            0.5532646897637169,
            0.28283121796858735,
            0.0023393446168564758,
            0.54027399425744,
            0.5054603742880899,
            0.8414459593178738,
            0.8888400513837226,
            0.6477615598506755,
            0.5463852474548928,
            0.22949832244208968,
            0.31690470916831925,
            0.33430272710595077,
            0.0018161848198701147,
            0.49139883689044017,
            0.6797887885789651,
            0.5861639317229459,
            0.035282919686503766,
            0.10547619370542094,
            0.7271339305759082,
            0.5016067603875377,
            0.8157682787043935,
            0.4560885642814806,
            0.6929899653460532,
            0.5490310236928294,
            0.4530007155322374,
            0.44049548891776324,
            0.3073456560671243,
            0.7919866041386854,
            0.9399916206326354,
            0.7527419916944253,
            0.10112484431007052,
            0.2243815401473951,
            0.07039176683656823,
            0.6169715161346244,
            0.08123319473394963,
            0.33386307361579226,
            0.7833580512725923,
            0.2505341520737143,
            0.30782230756455664,
            0.7479846627003448,
            0.2317759693352216,
            0.933348786188565,
            0.2750733182757722,
            0.7252317550631081,
            0.06447834497830485,
            0.032088850870325425,
            0.8873228725504719,
            0.15311031466542857,
            0.2687565257110237,
            0.004116203327798051,
            0.9334773509255564,
            0.4411582352055272,
            0.915699640538774,
            0.6540205822944993,
            0.22129013965221733,
            0.9197389724854845,
            0.7413607462811231,
            0.33557913718762133,
            0.48682708653142126,
            0.23261276643023798,
            0.7298006313346186,
            0.05533463818311113,
            0.5593987881884823,
            0.014043027025292698,
            0.8466474369742201,
            0.476037950820811,
            0.8444410961569767,
            0.16171101653838926,
            0.42377692467080386,
            0.9780260328429565,
            0.2702756266464895,
            0.6443988832387432,
            0.8993272243844188,
            0.7327024724314705,
            0.6054206077823142,
            0.588745513467837,
            0.6099464369729185,
            0.5242293066667245,
            0.6367910856473106,
            0.95096174945863,
            0.7939574092983276,
            0.3852863174746787,
            0.20255106392741273,
            0.4245303071023879,
            0.9967055343008029,
            0.4458664645461715,
            0.9257682958149203,
            0.3415930338870309,
            0.19630239658931292,
            0.5492196374119545,
            0.15687901061103193,
            0.6952090496329576,
            0.3956802166028377,
            0.5576427063755885,
            0.2040497200375847,
            0.5924369085502799,
            0.31777150634999896,
            0.6188717500402732,
            0.10233206975978115,
            0.6843201135492385,
            0.97041224730265,
            0.2995654214923589,
            0.6445544485180777,
            0.8301737642729924,
            0.1087683656777727,
            0.20019953992353745,
            0.22780836215874622,
            0.1276194439987589,
            0.38899213955877954,
            0.5064763107596547,
            0.6461906101897172,
            0.29746881883163756,
            0.12374457000837158,
            0.8069582603483658,
            0.25061357630791214,
            0.271756743228987,
            0.5239738641031194,
            0.6553247838521854,
            0.683817735836212,
            0.06802304767960465,
            0.7418517643983699,
            0.07774073352161648,
            0.9620799864233502,
            0.7358697033778088,
            0.6321490186410873,
            0.6177412024364181,
            0.868224449638578,
            0.7861974420960143,
            0.9434823199966517,
            0.6471369630040992,
            0.4048133530399751,
            0.21870779533881557,
            0.04034528679441152,
            0.40085799212995166,
            0.7373559016434784,
            0.8017355475036948,
            0.8967226537967593,
            0.7176831089070658,
            0.2613162507363975,
            0.3295930554915326,
            0.9184791699454984,
            0.640401400967848,
            0.8506077837886161,
            0.7664496800476533,
            0.1840288042109116,
            0.19215292024269792,
            0.9621850056142566,
            0.7270739851985532,
            0.4000222354425286,
            0.7398694017088451,
            0.8102704435115223,
            0.04869313905506856,
            0.9793000884909765,
            0.1741974674289617,
            0.3761252028210257,
            0.5186546213017674,
            0.6544630055785722,
            0.035486878857104,
            0.8302105873193061,
            0.027163249430116165,
            0.22238221247779077,
            0.6629828858480685,
            0.28167067761823206,
            0.15507531405462183,
            0.5299310060229058,
            0.17615517303353367,
            0.9160813969780187,
            0.21545598837949875,
            0.12327465612788735,
            0.22341207675958197,
            0.7350951031975246,
            0.20212244417713343,
            0.10806891511769379,
            0.07983934140633298,
            0.5784829838569209,
            0.378211476865968,
            0.1600334499763354,
            0.005458695509232947,
            0.04659586838146379,
            0.16941413051975984,
            0.8176494023491518,
            0.8193983869596879,
            0.8108230208574312,
            0.6743040619295617,
            0.9291104455200995,
            0.4551862464547173,
            0.5081503416196177,
            0.5863629766904948,
            0.879725318736957,
            0.44780357058335174,
            0.08000528315519806,
            0.7156738239702364,
            0.5579713809884808,
            0.3849056408121806,
            0.5439488769415571,
            0.4231699491641453,
            0.28212369837803397,
            0.5234097014488315,
            0.8952558680115784,
            0.5907776503878093,
            0.20310821526826173,
            0.36655334752448054,
            0.8022062978517467,
            0.28033812768504074,
            0.6691226037291831,
            0.7733590567458623,
            0.3259616467610902,
            0.6486620415911235,
            0.9052615556593607,
            0.9541189951513983,
            0.7111093017245231,
            0.824177966913593,
            0.3118026811100194,
            0.51957010835145,
            0.6783040667606364,
            0.2548584383539012,
            0.775639794655331,
            0.34912344674321705,
            0.5113227047977591,
            0.38981951977369,
            0.6214892598250877,
            0.004845542093178001,
            0.004956807664176277,
            0.33672401396540286,
            0.6019483398625038,
            0.5470295540948279,
            0.16163727213383083,
            0.31074927912001327,
            0.7678958430795156,
            0.6317779345503789,
            0.9512327678975153,
            0.46061166841382717,
            0.10696108239436053,
            0.381467056434603,
            0.8552430463509642,
            0.06439561868365684,
            0.12451070106137119,
            0.6305588706791049,
            0.40516850182186237,
            0.003916288600096851,
            0.26809866573936025,
            0.5283523433835026,
            0.8916385072682825,
            0.5637932289017291,
            0.5345383280816733,
            0.07945741563095476,
            0.4677977232825933,
            0.2180420111723671,
            0.6573840430041248,
            0.18553753661109285,
            0.012482076643287265,
            0.685607665579137,
            0.026974059044606813,
            0.6536573357598158,
            0.5502019862922036,
            0.5546464002416955,
            0.5321062302737098,
            0.7223743474389412,
            0.9466031287158234,
            0.7835158042814866,
            0.9456723815823406,
            0.8602920602906611,
            0.8689901181761116,
            0.9960072752618597,
            0.008499563604653315,
            0.832298969481983,
            0.37385951105474646,
            0.2766630334670427,
            0.35414551853848253,
            0.10147091413171239,
            0.44593584268151776,
            0.8478741461548431,
            0.174711009992771,
            0.29931914487221467,
            0.7374595528588062,
            0.6062871995304973,
            0.05456237473928949,
            0.8095786284527302,
            0.6711379698820543,
            0.7780227416082675,
            0.5698575810039058,
            0.8374210414415328,
            0.3421536429195372,
            0.5416694501192495,
            0.4345414149053174,
            0.0041291448761129335,
            0.12106504325643708,
            0.9729478194191092,
            0.2957285912729133,
            0.6734063562326669,
            0.32956672575040535,
            0.6941989745259985,
            0.39087545739740936,
            0.6285237359484402,
            0.5464775357369276,
            0.04355009567600665,
            0.29659575970461016,
            0.5061376740223799,
            0.09041273927723548,
            0.7292889822993045,
            0.9631242356697804,
            0.49001636433847895,
            0.3729672006767415,
            0.0402186820818361,
            0.06709255594189967,
            0.12055672632871284,
            0.6958349898364417,
            0.5168344061425046,
            0.9723494562157396,
            0.2768020892836862,
            0.8610575915312655,
            0.7795389307661503,
            0.1643268251583806,
            0.412251275774113,
            0.4955849398845882,
            0.4498738620874162,
            0.9636214177620601,
            0.09621299324389432,
            0.23941877187780947,
            0.8142114016710098,
            0.04011767557202495,
            0.8448948522047415,
            0.06508688407594121,
            0.1164247528356549,
            0.48177931620517767,
            0.030447745332007226,
            0.712141309568534,
            0.8902785075389523,
            0.44991611450123314,
            0.06202794456734484,
            0.5672075413811598,
            0.3724815418357975,
            0.5988538990093506,
            0.9866407611428497,
            0.6707747684273065,
            0.12620016729013617,
            0.38787793031078255,
            0.7718898918841123,
            0.7497504867080291,
            0.37030721375666986,
            0.4216214156243707,
            0.7528013955672017,
            0.26375981042113605,
            0.5955910637988521,
            0.073722423286946,
            0.9062905993144147,
            0.43474707761722586,
            0.5188590494104617,
            0.34246642189260734,
            0.34146401922917746,
            0.7885189916945865,
            0.17355374722805195,
            0.32886168894915657,
            0.9986868630489248,
            0.5417750460652352,
            0.744462980469815,
            0.8548589409072936,
            0.9716442844863626,
            0.4974343077698695,
            0.7760922947042525,
            0.7944167596266641,
            0.4928778233861939,
            0.8714672280850508,
            0.7891406408650357,
            0.11847785463381755,
            0.040171363401182436,
            0.028299207378359337,
            0.5586780482622543,
            0.34886663795266004,
            0.7099344140963683,
            0.8278707100372183,
            0.40841680139205205,
            0.22717483353945367,
            0.024642126536398412,
            0.1999884989286147,
            0.7682993947910368,
            0.4467044441952054,
            0.24941543162963653,
            0.3186497336994355,
            0.42566616386915146,
            0.06394087636847845,
            0.41221892621264355,
            0.7239998149149116,
            0.10263700729951386,
            0.9547246834071182,
            0.3504771138967774,
            0.4652280096801351,
            0.893819877320391,
            0.849888410579933,
            0.3422029148752527,
            0.6756666171603669,
            0.7157645146155942,
            0.5608365577577554,
            0.8169342505195489,
            0.8718381947664786,
            0.24315192131144703,
            0.3167124330729658,
            0.4359695688746591,
            0.8652427501010571,
            0.007244731613245792,
            0.7170792894729603,
            0.6366906405192163,
            0.40636730975020696,
            0.7724049513343092,
            0.04140455800237475,
            0.5565739537948702,
            0.32626353298657196,
            0.031511958736421763,
            0.7643753998335007,
            0.4051384102734069,
            0.18506632215576713,
            0.20762432059272773,
            0.8449985558223492,
            0.11777461293647384,
            0.25036959722091445,
            0.5560865084668252,
            0.8999275714064795,
            0.3714033649319849,
            0.7131232729916241,
            0.4214041397785607,
            0.2868553632670624,
            0.7576367387439932,
            0.135169590926147,
            0.06681060010943962,
            0.5466903263659443,
            0.1176693227529878,
            0.2642373085121925,
            0.08272031716148143,
            0.17071942595720246,
            0.3933979275630807,
            0.725986982480158,
            0.6823115648016137,
            0.37448258553278446,
            0.9193929709707876,
            0.9253058562898909,
            0.6022093621721928,
            0.15242080609822006,
            0.28374219703922376,
            0.9753381587492779,
            0.04746481883728326,
            0.25664431695643397,
            0.15539021587341173,
            0.7862337203184313,
            0.27574131085352216,
            0.9125493475582659,
            0.4230557954471307,
            0.7868471237898292,
            0.7151805984378823,
            0.7558284163545362,
            0.6892190014608551,
            0.06942146888586542,
            0.10266718139933051,
            0.6584439531793709,
            0.6037903104572208,
            0.8070197306982111,
            0.9465443219138302,
            0.9624363391526284,
            0.27380020601043753,
            0.46071553394157294,
            0.7591095060843289,
            0.9519670757359452,
            0.3032361639665756,
            0.48877456852536816,
            0.23597884517457612,
            0.5107030646252234,
            0.20055459161258038,
            0.09388146557794119,
            0.6746870338805814,
            0.584734389609156,
            0.17223641386296107,
            0.6671529009880626,
            0.6782652738638101,
            0.9629082587670728,
            0.4453057757752613,
            0.8191854208325298,
            0.3155184484281153,
            0.24310357163460306,
            0.2677050840923676,
            0.8506695537257243,
            0.5568953368620309,
            0.404051974119802,
            0.6977410257168541,
            0.8655026435822981,
            0.6094346023419185,
            0.2971758141470803,
            0.9583171645243728,
            0.5485752370307014,
            0.46227742900340474,
            0.5935123026790572,
            0.3023653692266124,
            0.7075259003887401,
            0.5516307411953051,
            0.17245473487528018,
            0.985520967370092,
            0.15238613683361613,
            0.5884373032730832,
            0.6483816290378118,
            0.6201418469386697,
            0.1322361382943461,
            0.9934621639718728,
            0.9781355751886759,
            0.2358141672103008,
            0.9282631573809376,
            0.9433712252291138,
            0.19114365959486102,
            0.3789557238185065,
            0.7949475415804416,
            0.16656266849309387,
            0.2261051183163817,
            0.8506784198446528,
            0.06489027887765886,
            0.8653835416561619,
            0.08671353337358623,
            0.854043504677798,
            0.11813182777366338,
            0.6238952061942427,
            0.7689563050256499,
            0.2774346379980891,
            0.33068522837337777,
            0.4261665169559953,
            0.3099483646782023,
            0.6193186881829519,
            0.9739056868446708,
            0.11586525141234083,
            0.6398109412587539,
            0.7251085509952001,
            0.4677220480238611,
            0.1523212370477175,
            0.44766112997883833,
            0.3173429343510642,
            0.9425945825618068,
            0.8219018790724759,
            0.0645605164193992,
            0.09485431523703491,
            0.3894183522316814,
            0.6093011450600736,
            0.878984960737678,
            0.029343395573226738,
            0.3324060238167921,
            0.4886808734964564,
            0.5156854980153162,
            0.14218370847598005,
            0.5344594796528955,
            0.18768022835138842,
            0.6497327847300925,
            0.3684397075864396,
            0.9193626028147847,
            0.6522977895048481,
            0.08962724254791887,
            0.5922688193985055,
            0.7356970039838325,
            0.5824259203362957,
            0.5407058235265322,
            0.5711451683211365,
            0.49565959637601587,
            0.5977429115752058,
            0.10924570433840974,
            0.22480770459645172,
            0.2871058305915194,
            0.2266596204129937,
            0.7738819468480213,
            0.17605223928789138,
            0.38475472364931584,
            0.18629263259774886,
            0.1807323566515121,
            0.8170521024950523,
            0.12208008613018384,
            0.005467015504290074,
            0.10910273820449712,
            0.6060511051567308,
            0.3434570300901455,
            0.00042444932756435794,
            0.8452782644730006,
            0.9714023607029164,
            0.37583231162140673,
            0.11442158503702637,
            0.96867187984804,
            0.6184568216803141,
            0.3118038927254134,
            0.9438646917460232,
            0.7614637047418356,
            0.5403571615757983,
            0.8389639104963986,
            0.6474181216179613,
            0.6002145037813064,
            0.6881183722357025,
            0.047934266407373305,
            0.2501358423963378,
            0.5416590133894971,
            0.0017290255627294693,
            0.6642777653367096,
            0.540981209017387,
            0.5647677211241512,
            0.35899883177363723,
            0.38419491129487704,
            0.1706402044865839,
            0.5809668876996945,
            0.3531159350243369,
            0.07469854012290267,
            0.6246702892852438,
            0.9643022681914122,
            0.7261306179469745,
            0.4144560462391834,
            0.36244445415993687,
            0.2558861740953887,
            0.5695408223196087,
            0.49036727984389294,
            0.16847584290780449,
            0.8195450223938221,
            0.07089507494394409,
            0.5942090735800623,
            0.9195293910931344,
            0.3638411671978604,
            0.7620942124631933,
            0.753028025483766,
            0.7893800196806074,
            0.16155119783118355,
            0.15624280494307863,
            0.5884210171713868,
            0.7877299121605147,
            0.46736221856235216,
            0.1075177932212199,
            0.10943159980480244,
            0.7449189094923113,
            0.9536679155443701,
            0.2438288221302346,
            0.2948772532797116,
            0.2811340878298155,
            0.7900588490997729,
            0.2331033682917616,
            0.7856198274591133,
            0.8004759863798926,
            0.5975198549591832,
            0.4293998770749423,
            0.2261563303429156,
            0.4908675014741476,
            0.6516101907788727,
            0.9849042526829336,
            0.5096053125861685,
            0.2524687595398467,
            0.9851444981174782,
            0.40827194856378135,
            0.19817984578370884,
            0.0567899929553356,
            0.23187270743000887,
            0.26135901720792143,
            0.08418116464410874,
            0.0785658190220071,
            0.9209075924087098,
            0.597140852146377,
            0.7469386786418659,
            0.733290194898679,
            0.4997152469553926,
            0.8008279336390877,
            0.5083788664229297,
            0.1759501822664441,
            0.727822384427287,
            0.3107953887336189,
            0.3263461902698255,
            0.2602514070443085,
            0.5896784464321209,
            0.9483301974243472,
            0.8834965514066166,
            0.22232780026925225,
            0.5096825539156082,
            0.5352893212972208,
            0.18860766379968918,
            0.8094037239862744,
            0.1945477950005433,
            0.3346403145575815,
            0.39807032343898285,
            0.8014796253216447,
            0.3525327782740121,
            0.10204756706208229,
            0.6549680247117429,
            0.1931072702615666,
            0.4008562149257491,
            0.25985247444643433,
            0.6846273489478197,
            0.040128872723685705,
            0.810217419170049,
            0.4727426697704351,
            0.6503758044743186,
            0.47353437971881507,
            0.778937139300587,
            0.7110721606372872,
            0.4540262500587219,
            0.9742712248510724,
            0.8712133108702252,
            0.6874239178482641,
            0.5127367450795323,
            0.3549059571467863,
            0.9954711217104326,
            0.16329220375994358,
            0.22488682737865584,
            0.04238837235610171,
            0.4797553309064344,
            0.39065154498280175,
            0.6429885955630378,
            0.49568347415307357,
            0.022575523424778088,
            0.961194228717609,
            0.9613236030742949,
            0.9123276267620781,
            0.3017018564004411,
            0.21606041602015058,
            0.8520552011529079,
            0.8661346671513412,
            0.8974966130669128,
            0.9091192570171245,
            0.3977199981220819,
            0.407665179605855,
            0.5989973603256267,
            0.8950455178181695,
            0.19226975006208935,
            0.8877488866890373,
            0.3058880482205242,
            0.39430566699018443,
            0.7111942756699692,
            0.9717304353935692,
            0.7953174032548256,
            0.6411039190462657,
            0.4183770103618606,
            0.45472310569528984,
            0.6292905925127901,
            0.8237179638721334,
            0.5209522377910386,
            0.2444280522514548,
            0.8176983127801919,
            0.2725176067143923,
            0.9322298522883796,
            0.22275268702206463,
            0.513242497886465,
            0.08604118049226273,
            0.47521992113068645,
            0.5243709544234121,
            0.17208119423584156,
            0.6655622046848257,
            0.6218443959425871,
            0.5226727984184025,
            0.15026648590545966,
            0.15125953123114178,
            0.4461976200696216,
            0.7948473402753538,
            0.3481285828584283,
            0.05300674182014431,
            0.1502646084491771,
            0.08540063807203002,
            0.9725510293110878,
            0.07053657732254148,
            0.9419090652947318,
            0.13506577135812226,
            0.5204099044846535,
            0.653488689141511,
            0.7710005906520486,
            0.5543106542237832,
            0.3842205681780151,
            0.9212065802958664,
            0.9271653521751265,
            0.985937112031812,
            0.8052550387811409,
            0.9540875736578137,
            0.05302964512587549,
            0.709320652650226,
            0.48105952476102876,
            0.3848453304921482,
            0.887370590062141,
            0.7946318913319451,
            0.2067211921271186,
            0.24186179375964567,
            0.1753371261689105,
            0.3700353275190944,
            0.0004962975712109463,
            0.3024470353450792,
            0.9107571026498392,
            0.6176365464203079,
            0.903725980632377,
            0.6396323654545216,
        ];
        let subdiag = vec_static![
            0.03514759598732997,
            0.9166162329394264,
            0.5920073429746628,
            0.03737502282782612,
            0.27541296782662794,
            0.35757716121142913,
            0.6802416588616755,
            0.7155740678046522,
            0.9649012116262692,
            0.11672125251026266,
            0.9199392299400145,
            0.04817171621171579,
            0.9307293351081974,
            0.4760605890414521,
            0.8545545249806196,
            0.19139397234856892,
            0.8017765737079704,
            0.9769789093224842,
            0.5608589872755957,
            0.18330323024368844,
            0.7059108114098925,
            0.48011278224461784,
            0.8806095547117393,
            0.41943394511355314,
            0.37253038596905974,
            0.4313419335715023,
            0.7803145989384619,
            0.019417776658885333,
            0.3266616106392123,
            0.8648268658307395,
            0.281834063555935,
            0.9958657655364979,
            0.7132082295208675,
            0.0907894892925506,
            0.748076919810375,
            0.3976930251985855,
            0.27650571177384575,
            0.03956621553665818,
            0.870979758528376,
            0.02177900264583299,
            0.09696722421897186,
            0.7156762119010204,
            0.47255821795062647,
            0.28423526759639406,
            0.2362274799311106,
            0.8357206203449294,
            0.44117178225566733,
            0.7160926363896872,
            0.9324907248737948,
            0.5620567520065424,
            0.9204444617400935,
            0.6483125256827211,
            0.8388013670815763,
            0.11787659695285657,
            0.6330966970250446,
            0.15906857751212133,
            0.023688684061386356,
            0.03861804582118156,
            0.11120087954397706,
            0.7888367298801179,
            0.40081391746255934,
            0.49441010134626695,
            0.32634190993787904,
            0.25140319280159973,
            0.6801348242915457,
            0.6868281418370772,
            0.40611738472575964,
            0.07828447213451328,
            0.6749380673181273,
            0.08926316605641849,
            0.12043114953075107,
            0.013241903881996309,
            0.6314587819785878,
            0.2359252644095119,
            0.9845051905574118,
            0.29696107568866137,
            0.4106419477189991,
            0.15123381021597337,
            0.3709176678784105,
            0.18195819174032302,
            0.8533596504653861,
            0.7510672174999008,
            0.21626704312676748,
            0.9636364489347745,
            0.6128308055692433,
            0.953115514029098,
            0.8436888435919734,
            0.2996352171373927,
            0.6372446318265245,
            0.2746728984555966,
            0.1037273047268431,
            0.7978236676924543,
            0.9484254202265332,
            0.6624929480737886,
            0.1133937333565549,
            0.33829316318797487,
            0.9223632567091015,
            0.5851974854049407,
            0.10409325590177565,
            0.8740115390408132,
            0.9845352406551445,
            0.13902919473775033,
            0.5031422886935428,
            0.04688375456992533,
            0.5081286565974175,
            0.868093991897646,
            0.17490335099705945,
            0.6540328561893354,
            0.8398081465083584,
            0.21693015392629467,
            0.2469186553171181,
            0.08429394697815695,
            0.352751452137246,
            0.10758030077795577,
            0.22700681314093718,
            0.2347731841807721,
            0.9977616327533844,
            0.8359041699873175,
            0.006618522134742899,
            0.43664918463005764,
            0.8210277077119603,
            0.2638399939782796,
            0.17336929801988665,
            0.10285997583877982,
            0.4652625971704971,
            0.2733273522826204,
            0.8574149574522787,
            0.9806185589394525,
            0.0015017390133097441,
            0.033430903824059666,
            0.30159631467225945,
            0.8974993026788298,
            0.6400720771489617,
            0.8978507974188904,
            0.2520999469590316,
            0.45836130177468615,
            0.6328879304823644,
            0.7423569927089957,
            0.7647616636938184,
            0.5064526550497733,
            0.9850250775854433,
            0.16694222041216755,
            0.4455501188059532,
            0.5374287739797512,
            0.3189502630832104,
            0.7215562186523863,
            0.15916949628148003,
            0.46432140332822636,
            0.9345412518790319,
            0.9020418859218371,
            0.9063125261934585,
            0.8776716709019419,
            0.7372809118435587,
            0.13416655716832815,
            0.20863558478898614,
            0.9601527516411504,
            0.04287960921174683,
            0.37755333952553993,
            0.3417720230505361,
            0.018645823306778442,
            0.6056383320364142,
            0.6514231553072014,
            0.7636607888909127,
            0.9636169157716151,
            0.9766562186982161,
            0.35309414208504075,
            0.666500807200095,
            0.7314451558204479,
            0.32899361692172513,
            0.4147820422080323,
            0.010638598007814415,
            0.75815558581836,
            0.02169815428383548,
            0.8393399189992057,
            0.5944394006298711,
            0.22382973848925802,
            0.3397652600088771,
            0.7015409578708514,
            0.7786099639559989,
            0.9043538590746677,
            0.02128322827623863,
            0.5587072619140682,
            0.04155163711549137,
            0.5996987443084926,
            0.553583814129375,
            0.3554407110197826,
            0.2744460862519005,
            0.7128777538871974,
            0.7530751205335345,
            0.9009495860810993,
            0.8917721418615082,
            0.9798925601886752,
            0.19342242305623314,
            0.024017196131949636,
            0.5245663591541098,
            0.13727729379295928,
            0.5491421206018336,
            0.060808975234664886,
            0.38472819295119587,
            0.6378674218418567,
            0.5227205485068104,
            0.8445990640792207,
            0.012436340335015283,
            0.7408733967606148,
            0.07054771013209749,
            0.07155631798187301,
            0.3109309497032946,
            0.8672898374299146,
            0.3583852631021335,
            0.5245779103895792,
            0.18802706385725088,
            0.7140966740493614,
            0.5499514336069355,
            0.09955949956945287,
            0.7245623053513952,
            0.17377343762547004,
            0.6595068015219847,
            0.8349315746460483,
            0.8113518945228055,
            0.5483076047004339,
            0.2756110290420781,
            0.3054178348722053,
            0.06871776701231946,
            0.3719726135350194,
            0.8287237621631821,
            0.8857687441690538,
            0.7100048919244728,
            0.39966266759915614,
            0.5316956873267635,
            0.5275213833268538,
            0.3615522954432596,
            0.2571877574661159,
            0.3199700071612991,
            0.8551425371306557,
            0.32048877323539016,
            0.02585267594661278,
            0.6839778709817063,
            0.6419014019329597,
            0.9408387544324863,
            0.40948833652960426,
            0.4543886322254115,
            0.135552823291479,
            0.0573359887606284,
            0.7641878684368038,
            0.6400819212712041,
            0.4034058258248284,
            0.048389281308018006,
            0.5084248649672614,
            0.6353549066600827,
            0.8206578550882837,
            0.044157301972724716,
            0.9774796963860074,
            0.17613366930197827,
            0.7236733379068533,
            0.4395954155931402,
            0.53220451402432,
            0.7394251944824503,
            0.5099726221171791,
            0.9068096835994949,
            0.8567380773152629,
            0.3990114830989736,
            0.5789177208783894,
            0.21703260425390858,
            0.09801724009974588,
            0.8261383241604257,
            0.1720131914688977,
            0.1741672387512948,
            0.21573167586527808,
            0.363995620056507,
            0.3877594862249947,
            0.7627538721891199,
            0.3028001552657369,
            0.6925192592005605,
            0.9983574500441812,
            0.4402506075402062,
            0.9969490201849028,
            0.883564743735146,
            0.5745948407978116,
            0.039390447079829194,
            0.648982540750061,
            0.8121077463283781,
            0.3078661764303069,
            0.5112013516016634,
            0.45855507485752867,
            0.8680067321374374,
            0.5878616980657485,
            0.04647126546747615,
            0.3469028573493742,
            0.7098480342833867,
            0.10437610961707922,
            0.7645967501135319,
            0.3358450730062015,
            0.9897160552499019,
            0.7369318977715539,
            0.416174494159927,
            0.8739296858827521,
            0.7469703290976271,
            0.2688365437717575,
            0.629024514537825,
            0.049646037576477675,
            0.5808053589412941,
            0.3030673768920322,
            0.5560270535501435,
            0.6428769952770576,
            0.0668502266998201,
            0.7596493050830692,
            0.8443113846787801,
            0.11146396526586733,
            0.6318800710105476,
            0.600856525368827,
            0.6752109718053634,
            0.5673512968959964,
            0.4574054679020503,
            0.10359220949390213,
            0.8382928237132409,
            0.14043612573194442,
            0.089990326035021,
            0.9132118395578155,
            0.6141894909830402,
            0.10621386006678057,
            0.29232241722433105,
            0.07661754069758586,
            0.2324111815248312,
            0.12446678249890808,
            0.8128595794855592,
            0.4006073821901792,
            0.4734387523433071,
            0.887065723190606,
            0.26632779471001966,
            0.8714787815141146,
            0.8471389583861026,
            0.685316691195084,
            0.6707247582581257,
            0.928579547545599,
            0.02553828614773368,
            0.599534398559242,
            0.2872291636088161,
            0.5187303634053566,
            0.6853633041230819,
            0.9801566043794827,
            0.28991992195404803,
            0.6133172505587892,
            0.33597328743115185,
            0.8121184044422642,
            0.17083229872471262,
            0.9513644781388463,
            0.35546101658313944,
            0.6423236811634249,
            0.7468201274076843,
            0.647302785857128,
            0.010738149004345554,
            0.7183851270529749,
            0.09435605574406558,
            0.9530985532147299,
            0.06728057507618279,
            0.8034456703633842,
            0.7979147696402938,
            0.3822105215671153,
            0.2297974374183981,
            0.44640217719557884,
            0.1883600083067679,
            0.04750218394092687,
            0.22170690949746585,
            0.4840046643798931,
            0.22475138267285244,
            0.4198932328501168,
            0.7889157743393862,
            0.15281044858523352,
            0.14673384971080916,
            0.006993889316575963,
            0.6364919809167784,
            0.4172781587709603,
            0.4491972187640223,
            0.16182920016249625,
            0.007403967991580274,
            0.691461320209579,
            0.5816637213029577,
            0.7522606715818594,
            0.0918933405438338,
            0.35159417994924447,
            0.7602447419723402,
            0.7142902647900448,
            0.6382243601647429,
            0.8480143370848189,
            0.6314392835621164,
            0.8439084708835212,
            0.11071350237055422,
            0.05782456734970254,
            0.6617906034954492,
            0.9185891532719831,
            0.12143371274043513,
            0.492750786634106,
            0.7853426435898732,
            0.4497887225806262,
            0.18635877906252618,
            0.019811422063421902,
            0.5934761906380646,
            0.4455857188059149,
            0.8096388796203312,
            0.10275649974036594,
            0.6257368006353998,
            0.575750202333855,
            0.8573719070085712,
            0.8239032358075691,
            0.34403591200202155,
            0.9345733962318697,
            0.625369461070786,
            0.43915860018762687,
            0.5899961835162477,
            0.053995956233354736,
            0.38046944755554934,
            0.9491278288233935,
            0.033130977568233466,
            0.17607535381374018,
            0.05801732382059033,
            0.4538755877525351,
            0.11743529785925255,
            0.6791318581799634,
            0.08544473560430244,
            0.5187497921796217,
            0.5889708254267537,
            0.9665003452472861,
            0.10418296172193109,
            0.0672448602119764,
            0.2342804383991538,
            0.7534895198422364,
            0.441835359111311,
            0.8080281548555065,
            0.8371361091120421,
            0.8671078767817112,
            0.9719590542686816,
            0.07419467888313469,
            0.43012106218645596,
            0.8088664789679649,
            0.13528519103218006,
            0.0798446432975134,
            0.013418704157530992,
            0.9794852288563065,
            0.9289867971403145,
            0.5690022331612405,
            0.8945759232383244,
            0.7853938914133286,
            0.7310762075200202,
            0.9859379728103466,
            0.831092060494062,
            0.8217409245038338,
            0.8999605479073557,
            0.05261025832698352,
            0.33623061838562873,
            0.7648537258952935,
            0.7659562194183969,
            0.8719329057090859,
            0.08419227378203165,
            0.3242045288837502,
            0.5171860173551537,
            0.019912782586444133,
            0.9384089169173252,
            0.5965042752440841,
            0.48278375926195205,
            0.16392438276287424,
            0.187140389551246,
            0.16916393839881505,
            0.13726551357673022,
            0.8269110011590474,
            0.6118271679991382,
            0.8152732754679466,
            0.666745307831142,
            0.6476883393802874,
            0.6220901203388768,
            0.38040332376345787,
            0.34115141071142563,
            0.7873648179135119,
            0.6664534365764685,
            0.9058029740954557,
            0.6484503546054443,
            0.764668715148312,
            0.9612701627106187,
            0.08976716760112169,
            0.7774948110004088,
            0.5308908425168403,
            0.4139821948135307,
            0.15163369419930572,
            0.11394098215608839,
            0.5513266492622274,
            0.09339060427254409,
            0.6481276388246241,
            0.598444020910348,
            0.8298448230916241,
            0.6727176112811568,
            0.38333435133740557,
            0.6468125790238676,
            0.5242080194390628,
            0.16316592649881434,
            0.6664818704126,
            0.48373523732095913,
            0.9604058684829354,
            0.2692523672866294,
            0.9064712993729671,
            0.9749843431565036,
            0.8257295231523102,
            0.5450919843847578,
            0.44285319458843475,
            0.7186843265960554,
            0.6843538986222125,
            0.5770108070727499,
            0.1483563341866252,
            0.5585015160821443,
            0.03159869315859776,
            0.7381257467734162,
            0.6274473768451448,
            0.9267372329435319,
            0.37363099006440503,
            0.47446843813525197,
            0.032772479544093236,
            0.00379906646971917,
            0.42616877659431984,
            0.6742650531619625,
            0.9648053952942566,
            0.25717093456208484,
            0.6552045037647151,
            0.8239025264272717,
            0.02591609051696553,
            0.1584841211968424,
            0.16818874642525816,
            0.5294395390101027,
            0.1642264759590285,
            0.4126302404299753,
            0.08988362194895594,
            0.4360622948309868,
            0.32436882717641535,
            0.4418212764871775,
            0.4604755504888851,
            0.9575949410941967,
            0.7175436604175588,
            0.29733768335110844,
            0.5293559499976667,
            0.4894719200579313,
            0.36939247060990377,
            0.1374347603269518,
            0.4693336639964696,
            0.5689248255490967,
            0.1925430841115714,
            0.4859926698559668,
            0.8005623663500411,
            0.18377993510143098,
            0.5730598356703445,
            0.2466855650149451,
            0.3778086183932944,
            0.8534163797973425,
            0.3958760269463045,
            0.46918329030046146,
            0.5961718951885366,
            0.5465884435695455,
            0.1339872295984894,
            0.9518564736255202,
            0.7290358177713954,
            0.2667982590176554,
            0.20064816906965677,
            0.9446972449968342,
            0.7832565372283562,
            0.4405421873350379,
            0.530088946997536,
            0.4725951296759483,
            0.0014663069930448414,
            0.19875745639800735,
            0.271404502879651,
            0.87258408818107,
            0.8344271403507775,
            0.84651528914761,
            0.9086951930453072,
            0.4873133597394722,
            0.05325920734817391,
            0.3828635353633184,
            0.05557465353817326,
            0.3814625184907462,
            0.4377099617929453,
            0.359395411391342,
            0.9886628059125666,
            0.17836199487456805,
            0.2924825293118364,
            0.858037965799582,
            0.21500863080861088,
            0.7264625195993915,
            0.8779249670674611,
            0.2000216508377537,
            0.4744403113140292,
            0.6925783675074796,
            0.687828393934673,
            0.8811510107307556,
            0.5873693485344339,
            0.9303459418450954,
            0.21704663826709047,
            0.18697461691318107,
            0.6343883180747689,
            0.6871257614783475,
            0.07313853559569883,
            0.830409517034205,
            0.060234730841782214,
            0.6871971775554103,
            0.403541702084807,
            0.9498998068464556,
            0.15592557291498454,
            0.4474366789947387,
            0.11848948493959344,
            0.6183179532225159,
            0.44094171296357565,
            0.5652525720091016,
            0.4446466973485682,
            0.3214135275545623,
            0.6010916819162343,
            0.19370814650434287,
            0.544230928868775,
            0.6118615221676226,
            0.8709265224511328,
            0.8950870194895194,
            0.2666189124135384,
            0.8820390336892018,
            0.647566638297354,
            0.2930025106494676,
            0.9484960225387983,
            0.708510551160303,
            0.22259188191974133,
            0.2270012493372623,
            0.8037648950839286,
            0.21209306011852014,
            0.13449963174741786,
            0.47387981509402244,
            0.3326571690695004,
            0.6691445517280875,
            0.3635737148819327,
            0.8098649333350489,
            0.22432743987894221,
            0.7891010532901155,
            0.9563360125823266,
            0.6989854294930989,
            0.02451751771421673,
            0.5153057236378842,
            0.7229025058224616,
            0.5081865221113376,
            0.16971912562831382,
            0.026240211120949763,
            0.6617582805876826,
            0.48737130022690356,
            0.026103944368583987,
            0.44381744169400816,
            0.7045794291220656,
            0.3652351288035923,
            0.16424098114074281,
            0.028751483462400862,
            0.8234836956782872,
            0.38853757267105626,
            0.6052144940177917,
            0.4897247651936114,
            0.3255237344699222,
            0.5513622088246961,
            0.13619547466300863,
            0.805436638429507,
            0.1845243360457135,
            0.8558996888545993,
            0.1557897878083101,
            0.08355102809063064,
            0.8458651963255258,
            0.432182232913888,
            0.9895611219947696,
            0.47153443081989654,
            0.8869894093118946,
            0.15559614616337059,
            0.9567507674202934,
            0.43519693875717824,
            0.06255791293620105,
            0.8513013588071069,
            0.14104499152728056,
            0.06623265234705933,
            0.858336978274352,
            0.17023080453003303,
            0.985181529598539,
            0.9367912077465413,
            0.6850447921424999,
            0.569567925927151,
            0.3574863857003564,
            0.3929414369619496,
            0.7798264977989812,
            0.11140464613840195,
            0.030617196786866252,
            0.8864366716402711,
            0.5785898432176089,
            0.643081257353523,
            0.9284374847655709,
            0.7001706302177434,
            0.6083818989887125,
            0.040026996801102666,
            0.8024670482256436,
            0.5944368875936563,
            0.6463789437698947,
            0.3066367398015226,
            0.3506669019695924,
            0.63431885164807,
            0.592319454812138,
            0.15094573781557963,
            0.340436339744575,
            0.804882873472292,
            0.44914911469387553,
            0.048470473475964004,
            0.3028203515471283,
            0.8865808880058462,
            0.016996466120910436,
            0.3588247157827066,
            0.44381366627273955,
            0.6133130243948027,
            0.6793732857893435,
            0.5937880447464559,
            0.9236333717131933,
            0.10620265902358594,
            0.6781272277941961,
            0.46206077738190166,
            0.5837411523988725,
            0.6838737935191735,
            0.8888571729522181,
            0.1117486127397801,
            0.5271394727235267,
            0.841582357601115,
            0.055683160653552055,
            0.7378009917988021,
            0.1345067804099308,
            0.023603067965801028,
            0.7956056827623649,
            0.3861304971070073,
            0.6100668109183262,
            0.6402060812881678,
            0.3340736360358295,
            0.512650267083197,
            0.6943621535616193,
            0.9673799816609256,
            0.8107827181347624,
            0.49841780499148625,
            0.7518770184506263,
            0.5565745335500417,
            0.9808723356822058,
            0.8068635360397197,
            0.908681262855838,
            0.9152605982597944,
            0.44866095133404205,
            0.9599851394933173,
            0.2934678845913977,
            0.7592374249514868,
            0.5291313480256107,
            0.5188340513175598,
            0.3463210956043563,
            0.16675722612875354,
            0.811258684871908,
            0.4144522597135534,
            0.3000106018374561,
            0.6486806688272189,
            0.7150432102084934,
            0.5626534553029507,
            0.8490672525306053,
            0.5975992482391111,
            0.4836329711391879,
            0.29236983269309746,
            0.9122909824387251,
            0.24101012619440676,
            0.28606135589771586,
            0.214284944484199,
            0.6987746413236384,
            0.8606187625525669,
            0.5680627754034978,
            0.5577448653156638,
            0.27292102034886667,
            0.4723605997197269,
            0.4008560087475681,
            0.023646571277147088,
            0.5516376105896065,
            0.19226168342556682,
            0.7485035347212252,
            0.47271107867679585,
            0.879161456599194,
            0.672627747030352,
            0.5166390976270142,
            0.6463033535971618,
            0.20387169812973893,
            0.0016747812894766234,
            0.029763819218643017,
            0.9021832815409186,
            0.18205733315974348,
            0.44566220678138957,
            0.5086824535884374,
            0.8622052399579576,
            0.8778492418306816,
            0.04147755982688972,
            0.0003300822774104928,
            0.2300233308570272,
            0.11638097773203415,
            0.8877912606239582,
            0.8223947699484698,
            0.9016308879256799,
            0.4471068691530159,
            0.0967523806018944,
            0.22988634800429852,
            0.30641596253986436,
            0.24355542901947214,
            0.13632875085756924,
            0.24647742569058617,
            0.5307455040750163,
            0.9863212880422773,
            0.918718097173517,
            0.9531872942401909,
            0.9846706539836511,
            0.010832008901674683,
            0.7874704045680089,
            0.42240650181801453,
            0.6122554366093659,
            0.7583106953085872,
            0.7931774645387552,
            0.03876987809799448,
            0.9824863334518148,
            0.8849963293165498,
            0.6846563583535574,
            0.17077550911827233,
            0.4472189707111268,
            0.3076696890971775,
            0.10452915885027414,
            0.8542979719186184,
            0.35567916294185664,
            0.41745038137207335,
            0.6803947523885654,
            0.4865145832835298,
            0.8890112117760177,
            0.8896159115424894,
            0.35791747143755714,
            0.7103716929172195,
            0.33545214090580033,
            0.7853990720028072,
            0.6047114987242408,
            0.6940787385341096,
            0.7999811177869283,
            0.9545612318219266,
            0.7691751897986325,
            0.3534218003313171,
            0.7164867468747648,
            0.6603465707298116,
            0.8149035048784745,
            0.4181618221513588,
            0.7210019025474951,
            0.15630788288921538,
            0.6132290075987188,
            0.5517610367620278,
            0.6277113065039732,
            0.39238074042319493,
            0.28377003769183407,
            0.5417866411605511,
            0.4566967099927354,
            0.5564426470099018,
            0.060868401764593405,
            0.6094880119699536,
            0.33370549844498154,
            0.14478677661254435,
            0.47878414441966277,
            0.9676546051816021,
            0.9881664521925151,
            0.41477979611479965,
            0.7831259314522806,
            0.5162035582913124,
            0.8740989440663438,
            0.1210324686662908,
            0.8286384476864525,
            0.5506716069892422,
            0.32863744747641355,
            0.07781119225183342,
            0.9444840881154352,
            0.30654969285100087,
            0.9839963670150201,
            0.198995514233535,
            0.2424161430409465,
            0.2640876580224849,
            0.13899252436716902,
            0.5320212354787701,
            0.13847175480439033,
            0.48587889178834587,
            0.29283891441097065,
            0.8263256175727217,
            0.11797632499159805,
            0.1443416658543194,
            0.3366947202678495,
            0.6603118455550755,
            0.32027750485155937,
            0.8465086212476037,
            0.20340544885996203,
            0.7926302215934362,
            0.2173652194945127,
            0.061374187687189496,
            0.1245294114075205,
            0.6911079151785875,
            0.36100282685823837,
            0.027576518401000838,
            0.31007214948205597,
            0.5956472548709824,
            0.0044205399575183435,
            0.29886063705375854,
            0.282423820051234,
            0.7321569261537294,
            0.3331502022181124,
            0.5543204711831691,
            0.8864507895811109,
            0.22522697958493698,
            0.051068413606819796,
            0.49820708734739494,
            0.28789124913862363,
            0.3921078283891909,
            0.8361533457723219,
            0.8116903276671275,
            0.4734334100328207,
            0.5059761491293135,
            0.5869857748739118,
            0.45295550704035903,
            0.3451473426273679,
            0.023617158864443488,
            0.5929015022396065,
            0.9304971171549896,
            0.1116490388957384,
            0.839607069069392,
            0.1475612833358515,
            0.3737390146619419,
            0.5277748973360896,
            0.4729791060341131,
            0.41423844610339855,
            0.1497186509723214,
            0.185069743330749,
            0.2896006671075433,
            0.23278840390731026,
            0.7485279424409984,
            0.2997849445534869,
            0.8647359103770899,
            0.49903346741276366,
            0.8348563902271234,
            0.1276644908715967,
            0.7367268375405149,
            0.45047362044769734,
            0.21333824304464144,
            0.7565022526983096,
            0.26033032435280856,
            0.41280765538307396,
            0.9228548390643795,
            0.5317233491114747,
            0.43788136452804827,
            0.2021074308947174,
            0.37643647889997844,
            0.5267542097405596,
            0.47938912629766406,
            0.6740194794509481,
            0.40081010192193645,
            0.9467340520851908,
            0.9943528870146244,
            0.02899210686599385,
            0.437152961618406,
            0.8113106881671761,
            0.20138449221768684,
            0.19906319473670275,
            0.8669149132765681,
            0.8257651884567754,
            0.7217443875783902,
            0.4853989704912177,
            0.14265772574321023,
            0.3114446963372245,
            0.6615457478785746,
            0.40723657815724,
            0.36869508529588135,
            0.06301641746332409,
            0.9311605327509352,
            0.744853501022084,
            0.22129519196477387,
            0.5935016131468461,
            0.8640573358547035,
            0.14708304719541654,
            0.16776139755404318,
            0.4437816365500199,
            0.7650892905039962,
            0.7687509868619725,
            0.5728038805423521,
            0.7968836078421103,
            0.5971615964003105,
            0.2556522728720847,
            0.37212254820872304,
            0.45580878740895203,
            0.824462777838793,
            0.4163502393144646,
            0.9577862104707457,
            0.1362484194994512,
            0.232791236677228,
            0.7785535014656395,
            0.5250141255249315,
            0.07796582605476632,
            0.8180435687781734,
            0.04904493687852163,
            0.09975917105814036,
            0.19042305196179277,
            0.4681031570408515,
            0.4185813964290853,
            0.7563440800937025,
            0.6938130124882698,
            0.0009878846110014106,
            0.5598787194431837,
            0.1188486188064396,
            0.6618904140253677,
            0.44247380503712164,
            0.575894868467275,
            0.018541030366061584,
            0.8931733797688469,
            0.4823910525159628,
            0.16914099091747758,
            0.6902171222226382,
            0.7497785583990529,
            0.5660572587171601,
            0.7237032190678135,
            0.8884497001412884,
            0.401976818736702,
            0.061561328160198325,
            0.17345963950113263,
            0.21741074761777046,
            0.664765026825943,
            0.0003142523234960226,
            0.49790557550355774,
            0.7972941049881628,
            0.4354805939118921,
            0.6289399489190155,
        ];

        let n = diag.len();
        let mut u = Mat::from_fn(n + 1, n + 1, |_, _| f64::NAN);
        let mut v = Mat::from_fn(n, n, |_, _| f64::NAN);
        let s = {
            let mut diag = diag.clone();
            let mut subdiag = subdiag.clone();
            compute_bidiag_real_svd(
                &mut diag,
                &mut subdiag,
                Some(u.as_mut()),
                Some(v.as_mut()),
                40,
                0,
                f64::EPSILON,
                f64::MIN_POSITIVE,
                Parallelism::None,
                make_stack!(bidiag_real_svd_req::<f64>(
                    n,
                    40,
                    true,
                    true,
                    Parallelism::None
                )),
            );
            Mat::from_fn(n + 1, n, |i, j| if i == j { diag[i] } else { 0.0 })
        };

        let reconstructed = &u * &s * v.transpose();
        for j in 0..n {
            for i in 0..n + 1 {
                let target = if i == j {
                    diag[j]
                } else if i == j + 1 {
                    subdiag[j]
                } else {
                    0.0
                };

                assert_approx_eq!(reconstructed.read(i, j), target, 1e-10);
            }
        }
    }
}
