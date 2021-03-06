// Copyright 2019 Karl Sundequist Blomdahl <karl.sundequist.blomdahl@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use go::util::sgf::SgfCoordinate;
use go::{Board, Color};
use parallel::spin::Mutex;
use mcts::asm::{argmax_f32, argmax_i32};
use util::{config, max};

use ordered_float::OrderedFloat;
use rand::{thread_rng, Rng};
use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr;

lazy_static! {
    /// Mapping from policy index to the `x` coordinate it represents.
    pub static ref X: Box<[u8]> = (0..361).map(|i| (i % 19) as u8).collect::<Vec<u8>>().into_boxed_slice();

    /// Mapping from policy index to the `y` coordinate it represents.
    pub static ref Y: Box<[u8]> = (0..361).map(|i| (i / 19) as u8).collect::<Vec<u8>>().into_boxed_slice();
}

/// An implementation of the _Polynomial UCT_ as suggested in the AlphaGo Zero
/// paper [1].
///
/// [1] https://www.nature.com/articles/nature24270
#[derive(Clone)]
pub struct PUCT;

impl PUCT {
    #[inline(always)]
    unsafe fn get_big_impl(node: &Node, big: &BigChildrenImpl, value: &mut [f32]) {
        use std::intrinsics::{fadd_fast, fdiv_fast, fmul_fast};

        let n = node.total_count + node.vtotal_count;
        let sqrt_n = ((1 + n) as f32).sqrt();
        let uct_exp = config::get_uct_exp(n);
        let uct_exp_sqrt_n = fmul_fast(uct_exp, sqrt_n);

        for i in 0..362 {
            let count = *big.count.get_unchecked(i) + *big.vcount.get_unchecked(i) as i32;
            let prior = *node.prior.get_unchecked(i);
            let value_ = *value.get_unchecked(i);
            let exp_bonus = fdiv_fast(uct_exp_sqrt_n, (1 + count) as f32);

            *value.get_unchecked_mut(i) = fadd_fast(value_, fmul_fast(prior, exp_bonus));
        }
    }

    #[allow(unused_attributes)]
    #[target_feature(enable = "avx,avx2")]
    unsafe fn get_big_avx2(node: &Node, big: &BigChildrenImpl, value: &mut [f32]) {
        PUCT::get_big_impl(node, big, value);
    }

    #[inline(always)]
    unsafe fn get_small_impl(node: &Node, small: &SmallChildrenImpl, value: &mut [f32]) {
        debug_assert!(SMALL_SIZE == 8);

        use std::intrinsics::{fadd_fast, fdiv_fast, fmul_fast};

        let n = node.total_count + node.vtotal_count;
        let sqrt_n = ((1 + n) as f32).sqrt();
        let uct_exp = config::get_uct_exp(n);
        let uct_exp_sqrt_n = fmul_fast(uct_exp, sqrt_n);

        for i in 0..362 {
            let other = small.find_index_fast(i);
            let prior = *node.prior.get_unchecked(i);
            let value_ = *value.get_unchecked(i);

            if let Some(other) = other {
                let count = small.count[other] + small.vcount[other] as i32;
                let exp_bonus = fdiv_fast(uct_exp_sqrt_n, (1 + count) as f32);

                *value.get_unchecked_mut(i) = fadd_fast(value_, fmul_fast(prior, exp_bonus));
            } else {
                *value.get_unchecked_mut(i) = fadd_fast(value_, fmul_fast(prior, uct_exp_sqrt_n));
            }
        }
    }

    #[allow(unused_attributes)]
    #[target_feature(enable = "avx,avx2")]
    unsafe fn get_small_avx2(node: &Node, small: &SmallChildrenImpl, value: &mut [f32]) {
        PUCT::get_small_impl(node, small, value);
    }

    /// Update the trace backwards with the given value (and color).
    ///
    /// # Arguments
    ///
    /// * `trace` -
    /// * `color` -
    /// * `value` -
    ///
    #[inline]
    unsafe fn update(trace: &NodeTrace, color: Color, value: f32) {
        use std::intrinsics::{fadd_fast, fsub_fast, fdiv_fast};

        for &(node, _, index) in trace.iter() {
            let value_ = if color == (*node).color { value } else { 1.0 - value };

            // incremental update of the average value and remove any additional
            // virtual losses we added to the node
            let _guard = (*node).lock.lock();

            (*node).total_count += 1;
            (*node).vtotal_count -= *config::VLOSS_CNT;
            (*node).children.with_mut(index, |mut child| {
                let prev_count = child.count();
                let prev_vcount = child.vcount();
                let prev_value = child.value();

                child.set_count(prev_count + 1);
                child.set_value(fadd_fast(prev_value, fdiv_fast(fsub_fast(value_, prev_value), (prev_count + 1) as f32)));
                child.set_vcount(prev_vcount - *config::VLOSS_CNT);
            }, (*node).initial_value);
        }
    }

    /// Optimized implementation of the PUCT value function.
    ///
    /// # Arguments
    ///
    /// * `node` -
    /// * `value` - the winrates to use in the calculations
    ///
    #[inline(always)]
    fn get(node: &Node, value: &mut [f32]) {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                match node.children {
                    ChildrenImpl::Small(ref small) => PUCT::get_small_avx2(node, small, value),
                    ChildrenImpl::Big(ref big) => PUCT::get_big_avx2(node, big, value),
                }
            }
        } else {
            unsafe {
                match node.children {
                    ChildrenImpl::Small(ref small) => PUCT::get_small_impl(node, small, value),
                    ChildrenImpl::Big(ref big) => PUCT::get_big_impl(node, big, value),
                }
            }
        }
    }
}

/// An implementation of the First-Play Urgency.
struct FPU;

impl FPU {
    unsafe fn apply_big_impl(value: &mut [f32], big: &BigChildrenImpl, fpu_reduce: f32) {
        use std::intrinsics::fsub_fast;

        for i in 0..368 {
            let count = *big.count.get_unchecked(i) + *big.vcount.get_unchecked(i) as i32;

            if count == 0 {
                *value.get_unchecked_mut(i) = max(0.0, fsub_fast(*value.get_unchecked(i), fpu_reduce));
            }
        }
    }

    #[target_feature(enable = "avx,avx2")]
    unsafe fn apply_big_avx2(value: &mut [f32], big: &BigChildrenImpl, fpu_reduce: f32) {
        FPU::apply_big_impl(value, big, fpu_reduce)
    }

    unsafe fn apply_small_impl(value: &mut [f32], small: &SmallChildrenImpl, fpu_reduce: f32) {
        use std::arch::x86_64::*;
        use std::intrinsics::fsub_fast;

        let indices = _mm_loadu_si128(&small.indices as *const i16 as *const _);

        for i in 0..362 {
            let eq = _mm_cmpeq_epi16(indices, _mm_set1_epi16(i as i16));
            let eq = _mm_movemask_epi8(eq) as u32;

            if eq == 0 || small.count[_mm_tzcnt_32(eq) as usize / 2] == 0 {
                *value.get_unchecked_mut(i) = max(0.0, fsub_fast(*value.get_unchecked(i), fpu_reduce));
            }
        }
    }

    #[target_feature(enable = "avx,avx2")]
    unsafe fn apply_small_avx2(value: &mut [f32], small: &SmallChildrenImpl, fpu_reduce: f32) {
        FPU::apply_small_impl(value, small, fpu_reduce)
    }

    /// Apply the first play urgency reduction to all elements in `value` if `count`
    /// is zero.
    ///
    /// # Arguments
    ///
    /// * `value` - the value of each element
    /// * `big` - The children storage as a big node.
    /// * `fpu_reduce` - the reduction to apply
    ///
    #[inline(always)]
    fn apply_big(value: &mut [f32], big: &BigChildrenImpl, fpu_reduce: f32) {
        if is_x86_feature_detected!("avx2") {
            unsafe { FPU::apply_big_avx2(value, big, fpu_reduce) }
        } else {
            unsafe { FPU::apply_big_impl(value, big, fpu_reduce) }
        }
    }

    /// Apply the first play urgency reduction to all elements in `value` if `count`
    /// is zero.
    ///
    /// # Arguments
    ///
    /// * `value` - the value of each element
    /// * `small` - The children storage as a small node.
    /// * `fpu_reduce` - the reduction to apply
    ///
    #[inline(always)]
    fn apply_small(value: &mut [f32], small: &SmallChildrenImpl, fpu_reduce: f32) {
        if is_x86_feature_detected!("avx2") {
            unsafe { FPU::apply_small_avx2(value, small, fpu_reduce) }
        } else {
            unsafe { FPU::apply_small_impl(value, small, fpu_reduce) }
        }
    }
}

/// Returns the weighted n:th percentile of the given array, and the sum of
/// all smaller elements.
///
/// # Arguments
///
/// * `array` -
/// * `n` -
///
fn percentile<I: Iterator<Item=i32>>(array: I, total: i32, n: f64) -> (i32, f64) {
    let mut copy = array.collect::<Vec<i32>>();
    copy.sort_unstable_by_key(|val| -val);

    // step forward in the array until we have accumulated the requested amount
    let max_value = (total as f64) * (1.0 - n);
    let mut so_far = 0.0;

    for val in copy.into_iter() {
        so_far += val as f64;

        if so_far >= max_value {
            return (val, so_far);
        }
    }

    unreachable!();
}

/// Flyweight structure used to contain the values of a single child in a `Node`. These
/// values should never be modified as they will **not** be synchronized back to the
/// origin structure.
pub struct Child {
    expanding: bool,
    count: i32,
    vcount: i16,
    ptr: *mut Node,
    value: f32
}

impl Child {
    /// Returns a default child, with the given `value`. This constructor is normally used
    /// when a sparse `Node` does not contain a child it was asked for.
    ///
    /// # Arguments
    ///
    /// * `value` - the value of the parent `Node`
    ///
    fn with_value(value: f32) -> Child {
        Child {
            count: 0,
            vcount: 0,
            value: value,
            expanding: false,
            ptr: ptr::null_mut()
        }
    }

    /// Returns a child that is initialized from a `SmallChildrenImpl` at the given `index`.
    ///
    /// # Arguments
    ///
    /// * `small` - the `SmallChildrenImpl` to initialize from
    /// * `index` - the sparse index in `SmallChildrenImpl` to initialize from
    ///
    fn from_small(small: &SmallChildrenImpl, index: usize) -> Child {
        Child {
            count: small.count[index],
            vcount: small.vcount[index],
            value: small.value[index],
            expanding: small.expanding[index],
            ptr: small.ptr[index]
        }
    }

    /// Returns a child that is initialized from a `BigChildrenImpl` at the given `index`.
    ///
    /// # Arguments
    ///
    /// * `big` - the `BigChildrenImpl` to initialize from
    /// * `index` - the dense index in `BigChildrenImpl` to initialize from
    ///
    fn from_big(big: &BigChildrenImpl, index: usize) -> Child {
        debug_assert!(index < 362, "{}", index);

        Child {
            count: big.count[index],
            vcount: big.vcount[index],
            value: big.value[index],
            expanding: big.expanding[index],
            ptr: big.ptr[index]
        }
    }

    /// Returns whether this child is currently being expanded.
    pub fn expanding(&self) -> bool {
        self.expanding
    }

    /// Returns the number of visits to this child.
    pub fn count(&self) -> i32 {
        self.count
    }

    /// Returns the number of virtual visits to this child.
    pub fn vcount(&self) -> i32 {
        self.vcount as i32
    }

    /// Return the child node itself.
    pub fn ptr(&self) -> *mut Node {
        self.ptr
    }

    /// Returns the average value of this child.
    pub fn value(&self) -> f32 {
        self.value
    }
}

/// Flyweight mutable structure used to contain the values of a single child in a `Node`.
#[derive(Debug)]
pub struct ChildMut {
    expanding: *mut bool,
    count: *mut i32,
    vcount: *mut i16,
    ptr: *mut *mut Node,
    value: *mut f32
}

impl ChildMut {
    /// Returns a child that is initialized from a `SmallChildrenImpl` at the given `index`. The
    /// given `small` node must outlive the returned `ChildMut`.
    ///
    /// # Arguments
    ///
    /// * `small` - the `SmallChildrenImpl` to initialize from
    /// * `index` - the sparse index in `SmallChildrenImpl` to initialize from
    ///
    unsafe fn from_small(small: &mut SmallChildrenImpl, index: usize) -> ChildMut {
        ChildMut {
            count: small.count.get_unchecked_mut(index),
            vcount: small.vcount.get_unchecked_mut(index),
            value: small.value.get_unchecked_mut(index),
            expanding: small.expanding.get_unchecked_mut(index),
            ptr: small.ptr.get_unchecked_mut(index)
        }
    }

    /// Returns a child that is initialized from a `BigChildrenImpl` at the given `index`. The
    /// given `big` node must outlive the returned `ChildMut`.
    ///
    /// # Arguments
    ///
    /// * `big` - the `BigChildrenImpl` to initialize from
    /// * `index` - the dense index in `BigChildrenImpl` to initialize from
    ///
    unsafe fn from_big(big: &mut BigChildrenImpl, index: usize) -> ChildMut {
        debug_assert!(index < 362);

        ChildMut {
            count: big.count.get_unchecked_mut(index),
            vcount: big.vcount.get_unchecked_mut(index),
            value: big.value.get_unchecked_mut(index),
            expanding: big.expanding.get_unchecked_mut(index),
            ptr: big.ptr.get_unchecked_mut(index)
        }
    }

    /// Returns whether this child is currently being expanded.
    pub fn expanding(&self) -> bool {
        unsafe { *self.expanding }
    }

    /// Returns the number of visits to this child.
    pub fn count(&self) -> i32 {
        unsafe { *self.count }
    }

    /// Returns the number of virtual visits to this child.
    pub fn vcount(&self) -> i32 {
        unsafe { *self.vcount as i32 }
    }

    /// Return the child node itself.
    pub fn ptr(&self) -> *mut Node {
        unsafe { *self.ptr }
    }

    /// Returns the average value of this child.
    pub fn value(&self) -> f32 {
        unsafe { *self.value }
    }

    /// Sets whether this child is, or has been, expanded.
    ///
    /// # Arguments
    ///
    /// * `value` - whether this child is expanding
    ///
    fn set_expanding(&mut self, value: bool) {
        unsafe { *self.expanding = value; }
    }

    /// Sets the number of visits to this child.
    ///
    /// # Arguments
    ///
    /// * `value` - the new number of visits to this child
    ///
    fn set_count(&mut self, value: i32) {
        unsafe { *self.count = value; }
    }

    /// Sets the number of virtual visits to this child.
    ///
    /// # Arguments
    ///
    /// * `value` - the new number of virtual visits to this child
    ///
    fn set_vcount(&mut self, value: i32) {
        unsafe { *self.vcount = value as i16; }
    }

    /// Sets the actual child node. If there is already a child node set, then you
    /// are responsible for freeing the old node.
    ///
    /// # Arguments
    ///
    /// * `value` - the new child `Node`
    ///
    fn set_ptr(&mut self, value: *mut Node) {
        unsafe { *self.ptr = value; }
    }

    /// Sets the average value of this child.
    ///
    /// # Arguments
    ///
    /// * `value` - the new average value of this child
    ///
    fn set_value(&mut self, value: f32) {
        unsafe { *self.value = value; }
    }
}

/// A dense representation of a `Node`.
#[repr(align(64))]
pub struct BigChildrenImpl {
    /// The number of times each edge has been traversed.
    pub count: [i32; 368],

    /// The number of virtual losses each edge has.
    pub vcount: [i16; 368],

    /// The average value for the sub-tree of each edge.
    pub value: [f32; 368],

    /// Whether some thread is currently busy (or is done) expanding the given
    /// child. This is used to avoid the same child being expanded multiple
    /// times by different threads.
    expanding: [bool; 362],

    /// The sub-tree that each edge points towards.
    ptr: [*mut Node; 362]
}

impl Drop for BigChildrenImpl {
    fn drop(&mut self) {
        for &child in self.ptr.iter() {
            if !child.is_null() {
                unsafe { Box::from_raw(child); }
            }
        }
    }
}

impl BigChildrenImpl {
    /// Returns a `BigChildrenImpl` that is equivalent to the given `small` node.
    ///
    /// # Arguments
    ///
    /// * `small` - the node to initialize from
    /// * `value` - the initial _value_ to use for any children not in `small`
    ///
    unsafe fn from_small(small: &SmallChildrenImpl, value: f32) -> BigChildrenImpl {
        let mut big = BigChildrenImpl {
            count: [0; 368],
            vcount: [0; 368],
            value: [value; 368],
            expanding: [false; 362],
            ptr: [ptr::null_mut(); 362]
        };

        for (index, &other) in small.indices.iter().enumerate() {
            if other >= 0 {
                let other = other as usize;

                big.count[other] = small.count[index];
                big.vcount[other] = small.vcount[index];
                big.value[other] = small.value[index];
                big.expanding[other] = small.expanding[index];
                big.ptr[other] = small.ptr[index];
            }
        }

        big
    }
}

/// The maximum number of elements a sparse (small) node contains.
const SMALL_SIZE: usize = 8;

/// Possible results for looking up an index in a small node.
enum SmallChildrenResult {
    Found(usize),
    NotFound(usize),
    Overflow
}

/// A sparse representation of a `Node` that only stores `SMALL_SIZE` children before
/// overflowing. It store the sparse indices in `indices`, which is an unsorted mapping
/// from sparse index to dense index.
pub struct SmallChildrenImpl {
    /// The number of times each edge has been traversed.
    pub count: [i32; SMALL_SIZE],

    /// The number of virtual losses each edge has.
    pub vcount: [i16; SMALL_SIZE],

    /// The average value for the sub-tree of each edge.
    pub value: [f32; SMALL_SIZE],

    /// Whether some thread is currently busy (or is done) expanding the given
    /// child. This is used to avoid the same child being expanded multiple
    /// times by different threads.
    expanding: [bool; SMALL_SIZE],

    /// The sub-tree that each edge points towards.
    ptr: [*mut Node; SMALL_SIZE],

    /// Indices of the children stored in this node.
    indices: [i16; SMALL_SIZE]
}

impl Drop for SmallChildrenImpl {
    fn drop(&mut self) {
        for &child in self.ptr.iter() {
            if !child.is_null() {
                unsafe { Box::from_raw(child); }
            }
        }
    }
}

impl SmallChildrenImpl {
    /// Returns an empty sparse node.
    ///
    /// # Arguments
    ///
    /// * `value` - the initial _value_ for any child
    ///
    fn with_value(value: f32) -> SmallChildrenImpl {
        SmallChildrenImpl {
            count: [0; SMALL_SIZE],
            vcount: [0; SMALL_SIZE],
            value: [value; SMALL_SIZE],
            expanding: [false; SMALL_SIZE],
            ptr: [ptr::null_mut(); SMALL_SIZE],
            indices: [::std::i16::MIN; SMALL_SIZE]
        }
    }

    /// Returns the sparse index for the given dense `index`, or `None` if it does
    /// not exist in this node.
    ///
    /// # Arguments
    ///
    /// * `index` - the index to search for
    ///
    #[inline(always)]
    unsafe fn find_index_fast(&self, index: usize) -> Option<usize> {
        use std::arch::x86_64::*;

        let indices = _mm_loadu_si128(&self.indices as *const i16 as *const _);
        let eq = _mm_cmpeq_epi16(indices, _mm_set1_epi16(index as i16));
        let eq = _mm_movemask_epi8(eq) as u32;

        if eq != 0 {
            let trailing_zeros = _mm_tzcnt_32(eq) as usize;

            Some(trailing_zeros / 2)
        } else {
            None
        }
    }

    /// Returns the first unused sparse index in this node, or `None` if this
    /// node is full.
    #[inline(always)]
    unsafe fn find_empty_fast(&self) -> Option<usize> {
        use std::arch::x86_64::*;

        let indices = _mm_loadu_si128(&self.indices as *const i16 as *const _);
        let eq = _mm_cmplt_epi16(indices, _mm_set1_epi16(0));
        let eq = _mm_movemask_epi8(eq) as u32;

        if eq != 0 {
            let trailing_zeros = _mm_tzcnt_32(eq) as usize;

            Some(trailing_zeros / 2)
        } else {
            None
        }
    }

    /// Returns the sparse index for the given dense `index`, or the index where it
    /// can be inserted.
    ///
    /// # Arguments
    ///
    /// * `index` - the index to search for
    ///
    fn find_index(&self, index: usize) -> SmallChildrenResult {
        match unsafe { self.find_index_fast(index) } {
            Some(index) => SmallChildrenResult::Found(index),
            None => {
                match unsafe { self.find_empty_fast() } {
                    Some(i) => SmallChildrenResult::NotFound(i),
                    _ => SmallChildrenResult::Overflow
                }
            }
        }
    }
}

/// Iterator over any non-zero child in a sparse node.
pub struct ChildrenNonZeroIter {
    count: *const i32,
    indices: *const i16,
    index: usize,
    len: usize
}

impl Iterator for ChildrenNonZeroIter {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        unsafe {
            while self.index < self.len {
                let prev_index = self.index;
                self.index += 1;

                if *self.count.add(prev_index) > 0 {
                    return Some(if self.indices.is_null() {
                        prev_index
                    } else {
                        *self.indices.add(prev_index) as usize
                    });
                }
            }

            None
        }
    }
}

/// Union of `SmallChildrenImpl` and `BigChildrenImpl`, where the later is stored on the heap.
pub enum ChildrenImpl {
    Small(ManuallyDrop<SmallChildrenImpl>),
    Big(Box<BigChildrenImpl>)
}

impl ChildrenImpl {
    /// Returns the scattered value array of all children.
    ///
    /// # Arguments
    ///
    /// * `default_value` - the value to use for any unvisited children
    ///
    pub fn value(&self, default_value: f32) -> Vec<f32> {
        match *self {
            ChildrenImpl::Big(ref big) => {
                big.value.to_vec()
            },
            ChildrenImpl::Small(ref small) => {
                let mut out = vec! [default_value; 368];

                for index in 0..SMALL_SIZE {
                    let other = small.indices[index];

                    if other >= 0 {
                        out[other as usize] = small.value[index];
                    }
                }

                out
            }
        }
    }

    /// Returns the index of the child with the largest number of visits.
    pub fn argmax_count(&self) -> usize {
        match *self {
            ChildrenImpl::Big(ref big) => argmax_i32(&big.count).unwrap(),
            ChildrenImpl::Small(ref small) => {
                let other = argmax_i32(&small.count).unwrap();
                let index = small.indices[other];

                if index < 0 {
                    0
                } else {
                    index as usize
                }
            }
        }
    }

    /// Returns the index of the child with the largest average value.
    pub fn argmax_value(&self) -> usize {
        match *self {
            ChildrenImpl::Big(ref big) => argmax_f32(&big.value).unwrap(),
            ChildrenImpl::Small(ref small) => {
                let other = argmax_f32(&small.value).unwrap();

                small.indices[other] as usize
            }
        }
    }

    /// Returns an iterator over all visited children.
    pub fn nonzero(&self) -> ChildrenNonZeroIter {
        match self {
            ChildrenImpl::Small(ref small) => {
                ChildrenNonZeroIter {
                    count: &small.count as *const i32,
                    indices: &small.indices as *const i16,
                    index: 0,
                    len: SMALL_SIZE
                }
            },
            ChildrenImpl::Big(ref big) => {
                ChildrenNonZeroIter {
                    count: &big.count as *const i32,
                    indices: ptr::null(),
                    index: 0,
                    len: 362
                }
            }
        }
    }

    /// Returns the result of the given callback, and being called with an immutable
    /// reference for the child for index.
    ///
    /// # Arguments
    ///
    /// * `index` -
    /// * `callback` -
    /// * `initial_value` -
    ///
    pub fn with<T, F>(&self, index: usize, callback: F, initial_value: f32) -> T
        where F: FnOnce(Child) -> T
    {
        callback(match self {
            ChildrenImpl::Small(ref small) => {
                match small.find_index(index) {
                    SmallChildrenResult::Found(other) => {
                        Child::from_small(small, other)
                    },
                    _ => {
                        Child::with_value(initial_value)
                    }
                }
            },
            ChildrenImpl::Big(ref big) => {
                Child::from_big(big, index)
            }
        })
    }

    /// Returns the result of the given callback, and being called with an mutable
    /// reference for the child for index.
    ///
    /// # Arguments
    ///
    /// * `index` -
    /// * `callback` -
    /// * `initial_value` -
    ///
    pub fn with_mut<T, F>(&mut self, index: usize, callback: F, initial_value: f32) -> T
        where F: FnOnce(ChildMut) -> T
    {
        let child = match self {
            ChildrenImpl::Small(ref mut small) => {
                match small.find_index(index) {
                    SmallChildrenResult::Found(other) => {
                        Some(unsafe { ChildMut::from_small(small, other) })
                    },
                    SmallChildrenResult::NotFound(other) => {
                        small.indices[other] = index as i16;

                        Some(unsafe { ChildMut::from_small(small, other) })
                    },
                    SmallChildrenResult::Overflow => {
                        None
                    }
                }
            },
            ChildrenImpl::Big(ref mut big) => {
                Some(unsafe { ChildMut::from_big(big, index) })
            }
        };

        if let Some(child) = child {
            callback(child)
        } else {
            *self = ChildrenImpl::Big(Box::new(unsafe {
                BigChildrenImpl::from_small(match self {
                    ChildrenImpl::Small(ref small) => small,
                    _ => unreachable!()
                }, initial_value)
            }));

            self.with_mut(index, callback, initial_value)
        }
    }
}

/// A monte carlo search tree.
#[repr(align(64))]
pub struct Node {
    /// Spinlock used to protect the data in this node during modifications.
    pub lock: Mutex,

    /// The color of each edge.
    pub color: Color,

    /// The initial vale of this node.
    pub initial_value: f32,

    /// The number of consecutive passes to reach this node.
    pub pass_count: i16,

    /// The total number of times any edge has been traversed.
    pub total_count: i32,

    /// The total number of virtual losses for any edge.
    pub vtotal_count: i32,

    /// The prior value of each edge as indicated by the policy.
    pub prior: [f32; 368],

    /// The sparse (or dense) representation of the remaining MCTS fields.
    pub children: ChildrenImpl
}

impl Drop for Node {
    fn drop(&mut self) {
        if let ChildrenImpl::Small(ref mut small) = self.children {
            unsafe { ManuallyDrop::drop(small) }
        }
    }
}

impl Node {
    /// Returns an empty search tree with the given starting color and prior
    /// values.
    ///
    /// # Arguments
    ///
    /// * `color` - the color of the first players color
    /// * `prior` - the prior values of the nodes
    ///
    pub fn new(color: Color, value: f32, prior: Vec<f32>) -> Node {
        assert!(prior.len() >= 362);

        // copy the prior values into an array size that is dividable
        // by 16 to ensure we can use 256-bit wide SIMD registers.
        let mut prior_padding = [::std::f32::NEG_INFINITY; 368];
        prior_padding[..362].copy_from_slice(&prior[..362]);

        Node {
            lock: Mutex::new(),
            color,
            initial_value: value,
            pass_count: 0,
            total_count: 0,
            vtotal_count: 0,
            prior: prior_padding,
            children: ChildrenImpl::Small(ManuallyDrop::new(SmallChildrenImpl::with_value(value)))
        }
    }

    /// Returns the total size of this search tree.
    pub fn size(&self) -> usize {
        self.total_count as usize
    }

    /// Returns the result of the given callback, and being called with an immutable
    /// reference for the child for index.
    ///
    /// # Arguments
    ///
    /// * `index` -
    /// * `callback` -
    ///
    pub fn with<T, F>(&self, index: usize, callback: F) -> T
        where F: FnOnce(Child) -> T
    {
        let _guard = self.lock.lock();

        self.children.with(index, callback, self.initial_value)
    }

    /// Returns the result of the given callback, and being called with an mutable
    /// reference for the child for index.
    ///
    /// # Arguments
    ///
    /// * `index` -
    /// * `callback` -
    ///
    pub fn with_mut<T, F>(&mut self, index: usize, callback: F) -> T
        where F: FnOnce(ChildMut) -> T
    {
        let _guard = self.lock.lock();

        self.children.with_mut(index, callback, self.initial_value)
    }

    fn as_sgf<S: SgfCoordinate>(&self, fmt: &mut fmt::Formatter, meta: bool) -> fmt::Result {
        // annotate the top-10 moves to make it easier to navigate for the
        // user.
        let mut children = (0..362).collect::<Vec<usize>>();
        children.sort_by_key(|&i| -self.with(i, |child| child.count()));

        if meta {
            for i in 0..10 {
                let j = children[i];

                if j != 361 && self.with(j, |child| child.count()) > 0 {
                    lazy_static! {
                        static ref LABELS: Vec<&'static str> = vec! [
                            "1", "2", "3", "4", "5", "6", "7", "8", "9", "10"
                        ];
                    }

                    write!(fmt, "LB[{}:{}]",
                        S::to_sgf(X[j] as usize, Y[j] as usize),
                        LABELS[i]
                    )?;
                }
            }

            // mark all valid moves with a triangle (for debugging the symmetry code)
            /*
            for i in 0..361 {
                if self.prior[i].is_finite() {
                    write!(fmt, "TR[{}]",
                        S::to_sgf(X[j] as usize, Y[j] as usize),
                    )?;
                }
            }
            */
        }

        let mut uct = self.children.value(self.initial_value);
        PUCT::get(self, &mut uct);

        for i in children {
            // do not output nodes that has not been visited to reduce the
            // size of the final SGF file.
            if self.with(i, |child| child.count()) == 0 {
                continue;
            }

            write!(fmt, "(")?;
            write!(fmt, ";{}[{}]",
                if self.color == Color::Black { "B" } else { "W" },
                if i == 361 { "tt".to_string() } else { S::to_sgf(X[i] as usize, Y[i] as usize) },
            )?;
            write!(fmt, "C[prior {:.4} value {:.4} (visits {} / total {}) uct {:.4}]",
                self.prior[i],
                self.with(i, |child| child.value()),
                self.with(i, |child| child.count()),
                self.total_count,
                uct[i]
            )?;

            unsafe {
                let child = self.with(i, |child| child.ptr());

                if !child.is_null() {
                    (*child).as_sgf::<S>(fmt, meta)?;
                }
            }

            write!(fmt, ")")?;
        }

        Ok(())
    }

    /// Returns the sub-tree that contains the exploration of the given move index.
    ///
    /// # Arguments
    ///
    /// * `self` - the search tree to pluck the child from
    /// * `index` - the move to pluck the sub-tree for
    ///
    pub fn forward(mut self, index: usize) -> Option<Node> {
        let color = self.color;
        let pass_count = self.pass_count;

        self.with_mut(index, |mut child| {
            if child.ptr().is_null() {
                if index == 361 {
                    // we need to record that were was a pass so that we have the correct
                    // pass count in the root node.
                    let prior = vec! [0.0f32; 362];
                    let mut next = Node::new(color.opposite(), 0.5, prior);
                    next.pass_count = pass_count + 1;

                    Some(next)
                } else {
                    None
                }
            } else {
                let next = child.ptr();
                child.set_ptr(ptr::null_mut());

                Some(unsafe { ptr::read(next) })
            }
        })
    }

    /// Returns the best move according to the current search tree. This is
    /// determined as the most visited child. If the temperature is non-zero
    /// then this process is stochastic, so that the probability that a move
    /// is picked is proportional to its visit count.
    ///
    /// # Arguments
    ///
    /// * `temperature` - How random the process should be, if set to +Inf
    ///   then the values are picked completely at random, and if set to 0
    ///   the selection is greedy.
    ///
    pub fn best(&self, temperature: f32) -> (f32, usize) {
        if temperature <= 9e-2 { // greedy
            let max_i = (0..362)
                .max_by_key(|&i| {
                    self.with(i, |child| {
                        (child.count(), OrderedFloat(self.prior[i]))
                    })
                })
                .unwrap();

            (self.with(max_i, |child| child.value()), max_i)
        } else {
            let t = (temperature as f64).recip();
            let c_total = self.children.nonzero().map(|i| self.with(i, |child| child.count())).sum::<i32>();
            let (c_threshold, c_total) = percentile(
                self.children.nonzero().map(|i| self.with(i, |child| child.count())),
                c_total,
                0.1
            );
            let mut s = vec! [::std::f64::NAN; 362];
            let mut s_total = 0.0;

            for i in self.children.nonzero() {
                let count = self.with(i, |child| child.count());

                if count >= c_threshold {
                    s_total += (count as f64 / c_total).powf(t);
                    s[i] = s_total;
                }
            }

            debug_assert!(s_total.is_finite());

            if s_total < ::std::f64::MIN_POSITIVE {
                (0.5, thread_rng().gen_range(0, 362))
            } else {
                let threshold = s_total * thread_rng().gen::<f64>();
                let max_i = (0..362).find(|&i| s[i] >= threshold).unwrap();

                (self.with(max_i, |child| child.value()), max_i)
            }
        }
    }

    /// Returns the best move according to the prior value of the root node.
    pub fn prior(&self) -> (f32, usize) {
        let max_i = argmax_f32(&self.prior).unwrap_or(361);

        (self.prior[max_i], max_i)
    }

    /// Returns a vector containing the _correct_ normalized probability that each move
    /// should be played given the current search tree.
    pub fn softmax<T: From<f32> + Clone>(&self) -> Vec<T> {
        let mut s = vec! [T::from(0.0f32); 362];
        let mut s_total = 0.0f32;

        for i in self.children.nonzero() {
            s_total += self.with(i, |child| child.count()) as f32;
        }

        for i in self.children.nonzero() {
            s[i] = T::from(self.with(i, |child| child.count()) as f32 / s_total);
        }

        s
    }

    /// Remove the given move as a valid choice in this search tree by setting
    /// its `value` to negative infinity.
    ///
    /// # Arguments
    ///
    /// * `index` - the index of the child to disqualify
    ///
    pub fn disqualify(&mut self, index: usize) {
        self.with_mut(index, |mut child| {
            child.set_value(::std::f32::NEG_INFINITY);
            child.set_count(0);
        });
    }

    /// Returns the child with the maximum UCT value, and increase its visit count
    /// by one.
    ///
    /// # Arguments
    ///
    /// * `apply_fpu` - whether to use the first-play urgency heuristic
    ///
    fn select(&mut self, apply_fpu: bool) -> Option<usize> {
        let mut value = {
            let _guard = self.lock.lock();

            self.children.value(self.initial_value)
        };

        if apply_fpu {
            // for unvisited children, attempt to transform the parent `value`
            // into a reasonable value for that child. This is known as the
            // _First Play Urgency_ heuristic, of the ones that has been tried
            // so far this one turns out to be the best:
            //
            // - square root visit count
            // - constant (this is currently used)
            // - zero
            //
            let fpu_reduce = config::get_fpu_reduce(self.total_count + self.vtotal_count);

            match self.children {
                ChildrenImpl::Big(ref big) => FPU::apply_big(&mut value, big, fpu_reduce),
                ChildrenImpl::Small(ref small) => FPU::apply_small(&mut value, small, fpu_reduce)
            }
        }

        // compute all UCB1 values for each node before trying to figure out which
        // to pick to make it possible to do it with SIMD.
        for i in 362..368 {
            value[i] = ::std::f32::NEG_INFINITY;
        }

        let _guard = self.lock.lock();
        PUCT::get(self, &mut value);

        // greedy selection based on the maximum ucb1 value, failing if someone else
        // is already expanding the node we want to expand.
        let initial_value = self.initial_value;
        let max_i = argmax_f32(&value).and_then(|i| {
            self.children.with(i, |child| {
                if child.expanding() && child.ptr().is_null() {
                    None  // someone else is already expanding this node
                } else {
                    Some(i)
                }
            }, initial_value)
        });

        if let Some(max_i) = max_i {
            self.vtotal_count += *config::VLOSS_CNT;
            self.children.with_mut(max_i, |mut child| {
                let prev_vcount = child.vcount();

                child.set_vcount(prev_vcount + *config::VLOSS_CNT);
                child.set_expanding(true);
            }, initial_value);
        }

        max_i
    }
}

pub type NodeTrace = Vec<(*mut Node, Color, usize)>;

/// Probe down the search tree, while updating the given board with the
/// moves the traversed edges represents, and return a list of the
/// edges. Which edges to traverse are determined according to the UCT
/// algorithm.
///
/// # Arguments
///
/// * `root` - the search tree to probe into
/// * `board` - the board to update with the traversed moves
///
pub unsafe fn probe(root: &mut Node, board: &mut Board) -> Option<NodeTrace> {
    let mut trace = vec! [];
    let mut current = root;

    loop {
        if let Some(next_child) = current.select(!trace.is_empty()) {
            trace.push((current as *mut Node, current.color, next_child));

            if next_child != 361 {  // not a passing move
                let (x, y) = (X[next_child] as usize, Y[next_child] as usize);

                debug_assert!(board.is_valid(current.color, x, y));
                board.place(current.color, x, y);
            } else if current.pass_count >= 1 {
                break;  // at least two consecutive passes
            }

            //
            let child = current.with(next_child, |child| child.ptr());

            if child.is_null() {
                break
            } else {
                current = &mut *child;
            }
        } else {
            // undo the entire trace, since we added virtual losses (optimistically)
            // on the way down.
            for (node, _, next_child) in trace.into_iter() {
                let _guard = (*node).lock.lock();

                (*node).vtotal_count -= *config::VLOSS_CNT as i32;
                (*node).children.with_mut(next_child, |mut child| {
                    let prev_vcount = child.vcount();

                    child.set_vcount(prev_vcount - *config::VLOSS_CNT);
                }, (*node).initial_value);
            }

            return None;
        }
    }

    Some(trace)
}

/// Insert a new node at the end of the given trace and perform the backup pass
/// updating the average and AMAF values of all nodes in the trace.
///
/// # Arguments
///
/// * `trace` -
/// * `color` -
/// * `value` -
/// * `prior` -
///
pub unsafe fn insert(trace: &NodeTrace, color: Color, value: f32, prior: Vec<f32>) {
    if let Some(&(node, _, index)) = trace.last() {
        let mut next = Box::new(Node::new(color, value, prior));
        if index == 361 {
            next.pass_count = (*node).pass_count + 1;
        }

        let updated = (*node).with_mut(index, |mut child| {
            if child.ptr().is_null() {
                child.set_ptr(Box::into_raw(next));
                true
            } else {
                false
            }
        });

        if !updated {
            debug_assert!(index == 361);

            // since we stop probing into a tree once two consecutive passes has
            // occurred we can double-expand those nodes. This is too prevent that
            // from causing memory leaks.
        }
    }

    PUCT::update(trace, color, value);
}

/// Type alias for `Node` that acts as a wrapper for calling `as_sgf` from
/// within a `write!` macro.
pub struct ToSgf<'a, S: SgfCoordinate> {
    _coordinate_format: ::std::marker::PhantomData<S>,
    starting_point: Board,
    root: &'a Node,
    meta: bool
}

impl<'a, S: SgfCoordinate> fmt::Display for ToSgf<'a, S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if self.meta {
            // add the standard SGF prefix
            write!(fmt, "(;GM[1]FF[4]SZ[19]RU[Chinese]KM[{:.1}]PL[{}]",
                self.starting_point.komi(),
                if self.root.color == Color::Black { "B" } else { "W" }
            )?;

            // write the starting point to the SGF file as pre-set variables
            for y in 0..19 {
                for x in 0..19 {
                    match self.starting_point.at(x, y) {
                        None => Ok(()),
                        Some(Color::Black) => write!(fmt, "AB[{}]", S::to_sgf(x, y)),
                        Some(Color::White) => write!(fmt, "AW[{}]", S::to_sgf(x, y))
                    }?
                }
            }

            // write the actual search tree
            self.root.as_sgf::<S>(fmt, self.meta)?;

            // add the standard SGF suffix
            write!(fmt, ")")
        } else {
            // write the actual search tree
            self.root.as_sgf::<S>(fmt, self.meta)
        }
    }
}

/// Returns a marker that contains all the examined positions of the given
/// search tree and can be displayed as an SGF file.
///
/// # Arguments
///
/// * `root` -
/// * `starting_point` -
/// * `meta` - whether to include the SGF meta data (rules, etc.)
///
pub fn to_sgf<'a, S>(root: &'a Node, starting_point: &Board, meta: bool) -> ToSgf<'a, S>
    where S: SgfCoordinate
{
    ToSgf {
        _coordinate_format: ::std::marker::PhantomData::default(),
        starting_point: starting_point.clone(),
        root: &root,
        meta: meta
    }
}

/// Type alias for pretty-printing an index based vertex.
struct PrettyVertex {
    inner: usize
}

impl fmt::Display for PrettyVertex {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if self.inner == 361 {
            fmt.pad("pass")
        } else {
            const LETTERS: [char; 19] = [
                'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'j', 'k',
                'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't'
            ];

            fmt.pad(&format!("{}{}",
                LETTERS[X[self.inner] as usize],
                Y[self.inner] + 1
            ))
        }
    }
}

/// Iterator that traverse the most likely path down a search tree
pub struct GreedyPath<'a> {
    current: &'a Node,
}

impl<'a> Iterator for GreedyPath<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let max_i = self.current.children.argmax_count();

        if self.current.with(max_i, |child| child.count()) == 0 {
            None
        } else {
            unsafe {
                self.current = &*self.current.with(max_i, |child| child.ptr());
            }

            Some(max_i)
        }
    }
}

/// Type alias for `Node` that acts as a wrapper for calling `as_sgf` from
/// within a `write!` macro.
pub struct ToPretty<'a> {
    root: &'a Node,
}

impl<'a> fmt::Display for ToPretty<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut children = self.root.children.nonzero().collect::<Vec<usize>>();
        children.sort_by_key(|&i| -self.root.with(i, |child| child.count()));

        if !*config::VERBOSE {
            children.truncate(10);
        }

        // print a summary containing the total tree size
        let total_value: f32 = (0..362)
            .map(|i| self.root.with(i, |child| child.count() as f32 * child.value()))
            .filter(|v| v.is_finite())
            .sum();
        let norm_value = total_value / (self.root.total_count as f32);
        let likely_path: String = GreedyPath { current: self.root }
                .map(|i| PrettyVertex { inner: i })
                .map(|v| format!("{}", v))
                .collect::<Vec<String>>().join(" ");

        writeln!(fmt, "Nodes: {}, Win: {:.1}%, PV: {}",
            self.root.total_count,
            100.0 * norm_value,
            likely_path
        )?;

        // print a summary of each move that we considered
        for i in children {
            let pretty_vertex = PrettyVertex { inner: i };
            let child = unsafe { &*self.root.with(i, |child| child.ptr()) };
            let likely_path: String = GreedyPath { current: child }
                    .map(|i| PrettyVertex { inner: i })
                    .map(|v| format!("{}", v))
                    .collect::<Vec<String>>().join(" ");

            writeln!(fmt, "{: >5} -> {:7} (W: {:5.2}%) (N: {:5.2}%) PV: {} {}",
                pretty_vertex,
                child.total_count,
                100.0 * self.root.with(i, |child| child.value()),
                100.0 * self.root.prior[i],
                pretty_vertex,
                likely_path
            )?;
        }

        Ok(())
    }
}

/// Returns a marker that contains all the examined positions of the given
/// search tree and can be pretty-printed to something easily examined by
/// a human.
///
/// # Arguments
///
/// * `root` -
/// * `starting_point` -
///
pub fn to_pretty(root: &Node) -> ToPretty {
    ToPretty { root: root }
}

#[cfg(test)]
mod tests {
    use rand::rngs::SmallRng;
    use rand::{Rng, SeedableRng};
    use test::Bencher;
    use go::*;
    use mcts::tree::*;
    use mcts::asm::sum_finite_f32;
    use mcts::asm::normalize_finite_f32;

    fn get_prior_distribution(rng: &mut SmallRng, board: &Board, color: Color) -> Vec<f32> {
        let mut prior: Vec<f32> = (0..368).map(|_| rng.gen::<f32>()).collect();
        let mut memoize = [0; 368];

        for i in 0..362 {
            if i != 361 && !board.is_valid_mut(color, X[i] as usize, Y[i] as usize, &mut memoize) {
                prior[i] = ::std::f32::NEG_INFINITY;
            }
        }

        let prior_sum = sum_finite_f32(&prior);
        normalize_finite_f32(&mut prior, prior_sum);

        prior
    }

    unsafe fn unsafe_visit_order() {
        let mut choices = vec! [];
        let mut rng = SmallRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        let mut root = Node::new(
            Color::Black,
            0.5,
            get_prior_distribution(&mut rng, &Board::new(DEFAULT_KOMI), Color::Black)
        );

        loop {
            let trace = probe(&mut root, &mut Board::new(DEFAULT_KOMI));

            if let Some(trace) = trace {
                assert_eq!(trace.len(), 1);

                // check that we are not re-visiting a node that we have not yet finished
                // expanding.
                let i = trace[0].2;

                assert!(!choices.contains(&i));
                choices.push(i);

                // check that the virtual loss has been correctly applied.
                assert_eq!(root.with(i, |child| child.vcount()), *config::VLOSS_CNT);
                assert_eq!(root.vtotal_count, choices.len() as i32 * *config::VLOSS_CNT as i32);

                // check that all nodes that were visited before this had larger prior
                // value.
                let prior_i = root.prior[i];

                for &other_i in &choices {
                    assert!(root.prior[other_i] >= prior_i);
                }
            } else {
                // check that we did not double-add any virtual loss
                for &other_i in &choices {
                    assert_eq!(root.with(other_i, |child| child.vcount()), *config::VLOSS_CNT);
                }

                assert_eq!(root.vtotal_count, choices.len() as i32 * *config::VLOSS_CNT as i32);
                break;
            }

            assert!(choices.len() < 362);
        }
    }

    #[test]
    fn visit_order() {
        unsafe { unsafe_visit_order() }
    }

    unsafe fn unsafe_virtual_loss() {
        let mut board = Board::new(DEFAULT_KOMI);
        let mut rng = SmallRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        let mut root = Node::new(
            Color::Black,
            0.5,
            get_prior_distribution(&mut rng, &board, Color::Black)
        );

        if let Some(trace) = probe(&mut root, &mut board) {
            let i = trace[0].2;

            // check that the virtual loss was applied
            assert_eq!(root.with(i, |child| child.vcount()), *config::VLOSS_CNT);
            assert_eq!(root.vtotal_count, *config::VLOSS_CNT as i32);

            // check that the virtual loss is un-applied after we update this move, and
            // that we we increase the `count` instead.
            let other_prior = get_prior_distribution(&mut rng, &board, Color::Black);
            let other_value = 0.9;

            insert(&trace, Color::Black, other_value, other_prior);

            assert_eq!(root.with(i, |child| child.vcount()), 0);
            assert_eq!(root.vtotal_count, 0);
            assert_eq!(root.with(i, |child| child.count()), 1);
            assert_eq!(root.total_count, 1);
        } else {
            panic!();
        }
    }

    #[test]
    fn virtual_loss() {
        unsafe { unsafe_virtual_loss() }
    }

    unsafe fn unsafe_value_update() {
        let mut board = Board::new(DEFAULT_KOMI);
        let mut root = Node::new(
            Color::Black,
            0.5,
            (0..362).map(|i| if i == 60 { 1.0 } else { 0.0 }).collect()
        );

        // to setup a scenario where we have two parallel probes that will both update
        // the same node value we need to pre-expand a node.
        let other_prior: Vec<f32> = (0..362).map(|i| if i == 61 || i == 62 { 0.5 } else { 0.0 }).collect();
        let trace = probe(&mut root, &mut board).unwrap();

        insert(&trace, Color::Black, 0.9, other_prior.clone());
        assert!({
            let value = root.with(60, |child| child.value());

            value >= 0.8999 && value <= 0.9001
        });
        assert_eq!(root.with(60, |child| child.count()), 1);
        assert_eq!(root.total_count, 1);
        assert_eq!(root.with(60, |child| child.vcount()), 0);
        assert_eq!(root.vtotal_count, 0);

        // two parallel probes in the same sub-tree.
        let trace_1 = probe(&mut root, &mut Board::new(DEFAULT_KOMI)).unwrap();
        let trace_2 = probe(&mut root, &mut Board::new(DEFAULT_KOMI)).unwrap();

        assert_eq!(trace_1[0].2, 60);
        assert_eq!(trace_2[0].2, 60);
        assert!(trace_1[1].2 == 61 || trace_2[1].2 == 61);
        assert!(trace_1[1].2 == 62 || trace_2[1].2 == 62);

        // the value of the root sub-tree should remain unchanged, but the virtual loss
        // should have increased.
        assert_eq!(root.with(60, |child| child.value()), 0.9);
        assert_eq!(root.with(60, |child| child.count()), 1);
        assert_eq!(root.total_count, 1);
        assert_eq!(root.with(60, |child| child.vcount()), 2 * *config::VLOSS_CNT);
        assert_eq!(root.vtotal_count, 2 * *config::VLOSS_CNT as i32);

        // check update after the first probe is inserted
        insert(&trace_1, Color::White, 0.2, other_prior.clone());

        assert_eq!(root.with(60, |child| child.value()), 0.85);
        assert_eq!(root.with(60, |child| child.count()), 2);
        assert_eq!(root.total_count, 2);
        assert_eq!(root.with(60, |child| child.vcount()), *config::VLOSS_CNT);
        assert_eq!(root.vtotal_count, *config::VLOSS_CNT as i32);

        // check update after the second probe is inserted
        insert(&trace_2, Color::White, 0.3, other_prior.clone());

        assert_eq!(root.with(60, |child| child.value()), 0.8);
        assert_eq!(root.with(60, |child| child.count()), 3);
        assert_eq!(root.total_count, 3);
        assert_eq!(root.with(60, |child| child.vcount()), 0);
        assert_eq!(root.vtotal_count, 0);
    }

    #[test]
    fn value_update() {
        unsafe { unsafe_value_update() }
    }

    unsafe fn unsafe_bench_probe_insert(b: &mut Bencher) {
        let lee_sedol_alphago_4_78 = [
            (Color::Black, 15,  3), (Color::White,  3, 15), (Color::Black,  2,  3), (Color::White, 16, 15),
            (Color::Black, 14, 15), (Color::White, 14, 16), (Color::Black, 13, 16), (Color::White, 15, 16),
            (Color::Black,  2, 13), (Color::White,  5, 16), (Color::Black, 12, 15), (Color::White, 15, 14),
            (Color::Black,  8, 16), (Color::White,  4,  2), (Color::Black,  7,  3), (Color::White,  2,  6),
            (Color::Black,  4,  3), (Color::White,  2,  9), (Color::Black,  3,  2), (Color::White,  1, 15),
            (Color::Black, 13,  2), (Color::White, 16,  8), (Color::Black,  4, 15), (Color::White,  4, 14),
            (Color::Black,  3, 10), (Color::White,  5, 15), (Color::Black,  2, 10), (Color::White,  3,  9),
            (Color::Black,  4,  9), (Color::White,  4,  8), (Color::Black,  5,  8), (Color::White,  4,  7),
            (Color::Black,  5,  7), (Color::White,  1,  9), (Color::Black,  5, 10), (Color::White,  5,  6),
            (Color::Black,  6,  6), (Color::White,  5,  5), (Color::Black,  6,  5), (Color::White, 12,  2),
            (Color::Black, 12,  3), (Color::White, 11,  2), (Color::Black, 13,  1), (Color::White,  8,  3),
            (Color::Black,  7,  2), (Color::White,  9,  6), (Color::Black, 15,  9), (Color::White, 15,  8),
            (Color::Black, 14,  9), (Color::White, 14,  8), (Color::Black, 13,  8), (Color::White, 13,  7),
            (Color::Black, 12,  7), (Color::White, 13,  6), (Color::Black, 12,  6), (Color::White, 12,  8),
            (Color::Black, 13,  9), (Color::White, 12,  5), (Color::Black, 11,  8), (Color::White, 13,  4),
            (Color::Black, 13,  3), (Color::White, 12,  9), (Color::Black, 11,  5), (Color::White, 12, 10),
            (Color::Black, 12,  4), (Color::White, 13,  5), (Color::Black, 11,  7), (Color::White, 16,  9),
            (Color::Black, 10, 10), (Color::White,  8, 10), (Color::Black,  9,  8), (Color::White,  6,  7),
            (Color::Black,  7,  9), (Color::White,  6,  4), (Color::Black,  7,  4), (Color::White,  5,  3),
            (Color::Black,  5,  2), (Color::White, 10,  8)
        ];

        let mut original_board = Board::new(DEFAULT_KOMI);

        for &(color, x, y) in lee_sedol_alphago_4_78.iter() {
            assert!(original_board.is_valid(color, x, y));

            original_board.place(color, x, y);
        }

        b.iter(|| {
            let mut rng = SmallRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
            let mut root = Node::new(
                Color::Black,
                0.5,
                get_prior_distribution(&mut rng, &original_board, Color::Black)
            );

            for _i in 0..800 {
                let mut board = original_board.clone();
                let trace = probe(&mut root, &mut board).unwrap();
                let next_color = board.last_played().map(|c| { c.opposite() }).unwrap_or(Color::Black);

                insert(
                    &trace,
                    next_color,
                    0.5,
                    get_prior_distribution(&mut rng, &board, next_color)
                );
            }

            root
        })
    }

    #[bench]
    fn bench_probe_insert(b: &mut Bencher) {
        unsafe { unsafe_bench_probe_insert(b) }
    }
}
