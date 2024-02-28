// Copyright 2024 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;

/// A data struct that holds either immutable string or reference to static str.
/// Compared to String or `Box<str>`, it avoids memory allocation on static str.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ImmutStr {
    Static(&'static str),
    Owned(Box<str>),
}

impl ImmutStr {
    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            ImmutStr::Static(s) => s,
            ImmutStr::Owned(s) => s.as_ref(),
        }
    }

    pub fn is_owned(&self) -> bool {
        match self {
            ImmutStr::Static(_) => false,
            ImmutStr::Owned(_) => true,
        }
    }
}

impl fmt::Display for ImmutStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<&'static str> for ImmutStr {
    fn from(s: &'static str) -> Self {
        ImmutStr::Static(s)
    }
}

impl From<String> for ImmutStr {
    fn from(s: String) -> Self {
        ImmutStr::Owned(s.into_boxed_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_vs_owned() {
        let s: ImmutStr = "test".into();
        assert!(!s.is_owned());
        let s: ImmutStr = "test".to_string().into();
        assert!(s.is_owned());
    }
}
