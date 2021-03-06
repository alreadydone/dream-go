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

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

pub struct MutexGuard<'a> {
    mutex: &'a Mutex
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        let previous = self.mutex.is_available.compare_and_swap(false, true, Ordering::SeqCst);

        assert_eq!(previous, false);
    }
}

/// A lock that provides mutual access using a spinlock algorithm, this makes
/// it suitable for locks that will only be held for *very brief* periods of
/// time.
pub struct Mutex {
    is_available: AtomicBool
}

impl Mutex {
    /// Returns an unlocked mutex.
    pub fn new() -> Mutex {
        Mutex { is_available: AtomicBool::new(true) }
    }

    #[inline]
    pub fn lock(&self) -> MutexGuard {
        while !self.is_available.compare_and_swap(true, false, Ordering::SeqCst) {
            thread::yield_now();
        }

        MutexGuard { mutex: self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock() {
        let mutex = Mutex::new();

        mutex.lock();
    }
}
