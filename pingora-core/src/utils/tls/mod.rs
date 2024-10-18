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

#[cfg(feature = "openssl_derived")]
mod boringssl_openssl;

#[cfg(feature = "openssl_derived")]
pub use boringssl_openssl::*;

#[cfg(feature = "rustls")]
mod rustls;

#[cfg(feature = "rustls")]
pub use rustls::*;
