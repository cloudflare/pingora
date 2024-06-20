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

//! Modules for HTTP traffic.
//!
//! [HttpModule]s define request and response filters to use while running an [HttpServer]
//! application.
//! See the [ResponseCompression] module for an example of how to implement a basic module.

pub mod compression;

use async_trait::async_trait;
use bytes::Bytes;
use once_cell::sync::OnceCell;
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use std::any::Any;
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

/// The trait an HTTP traffic module needs to implement
#[async_trait]
pub trait HttpModule {
    async fn request_header_filter(&mut self, _req: &mut RequestHeader) -> Result<()> {
        Ok(())
    }

    async fn request_body_filter(
        &mut self,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn response_header_filter(
        &mut self,
        _resp: &mut ResponseHeader,
        _end_of_stream: bool,
    ) -> Result<()> {
        Ok(())
    }

    fn response_body_filter(
        &mut self,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
    ) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub type Module = Box<dyn HttpModule + 'static + Send + Sync>;

/// Trait to init the http module ctx for each request
pub trait HttpModuleBuilder {
    /// The order the module will run
    ///
    /// The lower the value, the later it runs relative to other filters.
    /// If the order of the filter is not important, leave it to the default 0.
    fn order(&self) -> i16 {
        0
    }

    /// Initialize and return the per request module context
    fn init(&self) -> Module;
}

pub type ModuleBuilder = Box<dyn HttpModuleBuilder + 'static + Send + Sync>;

/// The object to hold multiple http modules
pub struct HttpModules {
    modules: Vec<ModuleBuilder>,
    module_index: OnceCell<Arc<HashMap<TypeId, usize>>>,
}

impl HttpModules {
    /// Create a new [HttpModules]
    pub fn new() -> Self {
        HttpModules {
            modules: vec![],
            module_index: OnceCell::new(),
        }
    }

    /// Add a new [ModuleBuilder] to [HttpModules]
    ///
    /// Each type of [HttpModule] can be only added once.
    /// # Panic
    /// Panic if any [HttpModule] is added more than once.
    pub fn add_module(&mut self, builder: ModuleBuilder) {
        if self.module_index.get().is_some() {
            // We use a shared module_index the index would be out of sync if we
            // add more modules.
            panic!("cannot add module after ctx is already built")
        }
        self.modules.push(builder);
        // not the most efficient way but should be fine
        // largest order first
        self.modules.sort_by_key(|m| -m.order());
    }

    /// Build the contexts of all the modules added to this [HttpModules]
    pub fn build_ctx(&self) -> HttpModuleCtx {
        let module_ctx: Vec<_> = self.modules.iter().map(|b| b.init()).collect();
        let module_index = self
            .module_index
            .get_or_init(|| {
                let mut module_index = HashMap::with_capacity(self.modules.len());
                for (i, c) in module_ctx.iter().enumerate() {
                    let exist = module_index.insert(c.as_any().type_id(), i);
                    if exist.is_some() {
                        panic!("duplicated filters found")
                    }
                }
                Arc::new(module_index)
            })
            .clone();

        HttpModuleCtx {
            module_ctx,
            module_index,
        }
    }
}

/// The Contexts of multiple modules
///
/// This is the object that will apply all the included modules to a certain HTTP request.
/// The modules are ordered according to their `order()`.
pub struct HttpModuleCtx {
    // the modules in the order of execution
    module_ctx: Vec<Module>,
    // find the module in the vec with its type ID
    module_index: Arc<HashMap<TypeId, usize>>,
}

impl HttpModuleCtx {
    /// Create a placeholder empty [HttpModuleCtx].
    ///
    /// [HttpModules] should be used to create nonempty [HttpModuleCtx].
    pub fn empty() -> Self {
        HttpModuleCtx {
            module_ctx: vec![],
            module_index: Arc::new(HashMap::new()),
        }
    }

    /// Get a ref to [HttpModule] if any.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        let idx = self.module_index.get(&TypeId::of::<T>())?;
        let ctx = &self.module_ctx[*idx];
        Some(
            ctx.as_any()
                .downcast_ref::<T>()
                .expect("type should always match"),
        )
    }

    /// Get a mut ref to [HttpModule] if any.
    pub fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        let idx = self.module_index.get(&TypeId::of::<T>())?;
        let ctx = &mut self.module_ctx[*idx];
        Some(
            ctx.as_any_mut()
                .downcast_mut::<T>()
                .expect("type should always match"),
        )
    }

    /// Run the `request_header_filter` for all the modules according to their orders.
    pub async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
        for filter in self.module_ctx.iter_mut() {
            filter.request_header_filter(req).await?;
        }
        Ok(())
    }

    /// Run the `request_body_filter` for all the modules according to their orders.
    pub async fn request_body_filter(
        &mut self,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<()> {
        for filter in self.module_ctx.iter_mut() {
            filter.request_body_filter(body, end_of_stream).await?;
        }
        Ok(())
    }

    /// Run the `response_header_filter` for all the modules according to their orders.
    pub async fn response_header_filter(
        &mut self,
        req: &mut ResponseHeader,
        end_of_stream: bool,
    ) -> Result<()> {
        for filter in self.module_ctx.iter_mut() {
            filter.response_header_filter(req, end_of_stream).await?;
        }
        Ok(())
    }

    /// Run the `response_body_filter` for all the modules according to their orders.
    pub fn response_body_filter(
        &mut self,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<()> {
        for filter in self.module_ctx.iter_mut() {
            filter.response_body_filter(body, end_of_stream)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MyModule;
    #[async_trait]
    impl HttpModule for MyModule {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
            req.insert_header("my-filter", "1")
        }
    }
    struct MyModuleBuilder;
    impl HttpModuleBuilder for MyModuleBuilder {
        fn order(&self) -> i16 {
            1
        }

        fn init(&self) -> Module {
            Box::new(MyModule)
        }
    }

    struct MyOtherModule;
    #[async_trait]
    impl HttpModule for MyOtherModule {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
            if req.headers.get("my-filter").is_some() {
                // if this MyOtherModule runs after MyModule
                req.insert_header("my-filter", "2")
            } else {
                // if this MyOtherModule runs before MyModule
                req.insert_header("my-other-filter", "1")
            }
        }
    }
    struct MyOtherModuleBuilder;
    impl HttpModuleBuilder for MyOtherModuleBuilder {
        fn order(&self) -> i16 {
            -1
        }

        fn init(&self) -> Module {
            Box::new(MyOtherModule)
        }
    }

    #[test]
    fn test_module_get() {
        let mut http_module = HttpModules::new();
        http_module.add_module(Box::new(MyModuleBuilder));
        http_module.add_module(Box::new(MyOtherModuleBuilder));
        let mut ctx = http_module.build_ctx();
        assert!(ctx.get::<MyModule>().is_some());
        assert!(ctx.get::<MyOtherModule>().is_some());
        assert!(ctx.get::<usize>().is_none());
        assert!(ctx.get_mut::<MyModule>().is_some());
        assert!(ctx.get_mut::<MyOtherModule>().is_some());
        assert!(ctx.get_mut::<usize>().is_none());
    }

    #[tokio::test]
    async fn test_module_filter() {
        let mut http_module = HttpModules::new();
        http_module.add_module(Box::new(MyOtherModuleBuilder));
        http_module.add_module(Box::new(MyModuleBuilder));
        let mut ctx = http_module.build_ctx();
        let mut req = RequestHeader::build("Get", b"/", None).unwrap();
        ctx.request_header_filter(&mut req).await.unwrap();
        // MyModule runs before MyOtherModule
        assert_eq!(req.headers.get("my-filter").unwrap(), "2");
        assert!(req.headers.get("my-other-filter").is_none());
    }
}
