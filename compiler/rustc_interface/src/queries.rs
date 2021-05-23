use crate::interface::{Compiler, Result};
use crate::passes::{self, BoxedResolver, QueryContext};

use rustc_ast as ast;
use rustc_codegen_ssa::traits::CodegenBackend;
use rustc_data_structures::steal::Steal;
use rustc_data_structures::svh::Svh;
use rustc_data_structures::sync::{Lrc, OnceCell, WorkerLocal};
use rustc_errors::ErrorReported;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_incremental::DepGraphFuture;
use rustc_lint::LintStore;
use rustc_middle::arena::Arena;
use rustc_middle::dep_graph::DepGraph;
use rustc_middle::ty::{GlobalCtxt, TyCtxt};
use rustc_query_impl::Queries as TcxQueries;
use rustc_serialize::json;
use rustc_session::config::{self, OutputFilenames, OutputType};
use rustc_session::{output::find_crate_name, Session};
use rustc_span::symbol::sym;
use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

/// Represent the result of a query.
///
/// This result can be stolen with the [`take`] method and generated with the [`compute`] method.
///
/// [`take`]: Self::take
/// [`compute`]: Self::compute
pub struct Query<T> {
    result: RefCell<Option<Result<T>>>,
}

impl<T> Query<T> {
    fn compute<F: FnOnce() -> Result<T>>(&self, f: F) -> Result<&Query<T>> {
        let mut result = self.result.borrow_mut();
        if result.is_none() {
            *result = Some(f());
        }
        result.as_ref().unwrap().as_ref().map(|_| self).map_err(|err| *err)
    }

    /// Takes ownership of the query result. Further attempts to take or peek the query
    /// result will panic unless it is generated by calling the `compute` method.
    pub fn take(&self) -> T {
        self.result.borrow_mut().take().expect("missing query result").unwrap()
    }

    /// Borrows the query result using the RefCell. Panics if the result is stolen.
    pub fn peek(&self) -> Ref<'_, T> {
        Ref::map(self.result.borrow(), |r| {
            r.as_ref().unwrap().as_ref().expect("missing query result")
        })
    }

    /// Mutably borrows the query result using the RefCell. Panics if the result is stolen.
    pub fn peek_mut(&self) -> RefMut<'_, T> {
        RefMut::map(self.result.borrow_mut(), |r| {
            r.as_mut().unwrap().as_mut().expect("missing query result")
        })
    }
}

impl<T> Default for Query<T> {
    fn default() -> Self {
        Query { result: RefCell::new(None) }
    }
}

pub struct Queries<'tcx> {
    compiler: &'tcx Compiler,
    gcx: OnceCell<GlobalCtxt<'tcx>>,
    queries: OnceCell<TcxQueries<'tcx>>,

    arena: WorkerLocal<Arena<'tcx>>,
    hir_arena: WorkerLocal<rustc_ast_lowering::Arena<'tcx>>,

    dep_graph_future: Query<Option<DepGraphFuture>>,
    parse: Query<ast::Crate>,
    crate_name: Query<String>,
    register_plugins: Query<(ast::Crate, Lrc<LintStore>)>,
    expansion: Query<(ast::Crate, Steal<Rc<RefCell<BoxedResolver>>>, Lrc<LintStore>)>,
    dep_graph: Query<DepGraph>,
    prepare_outputs: Query<OutputFilenames>,
    global_ctxt: Query<QueryContext<'tcx>>,
    ongoing_codegen: Query<Box<dyn Any>>,
}

impl<'tcx> Queries<'tcx> {
    pub fn new(compiler: &'tcx Compiler) -> Queries<'tcx> {
        Queries {
            compiler,
            gcx: OnceCell::new(),
            queries: OnceCell::new(),
            arena: WorkerLocal::new(|_| Arena::default()),
            hir_arena: WorkerLocal::new(|_| rustc_ast_lowering::Arena::default()),
            dep_graph_future: Default::default(),
            parse: Default::default(),
            crate_name: Default::default(),
            register_plugins: Default::default(),
            expansion: Default::default(),
            dep_graph: Default::default(),
            prepare_outputs: Default::default(),
            global_ctxt: Default::default(),
            ongoing_codegen: Default::default(),
        }
    }

    fn session(&self) -> &Lrc<Session> {
        &self.compiler.sess
    }
    fn codegen_backend(&self) -> &Lrc<Box<dyn CodegenBackend>> {
        &self.compiler.codegen_backend()
    }

    fn dep_graph_future(&self) -> Result<&Query<Option<DepGraphFuture>>> {
        self.dep_graph_future.compute(|| {
            let sess = self.session();
            Ok(sess.opts.build_dep_graph().then(|| rustc_incremental::load_dep_graph(sess)))
        })
    }

    pub fn parse(&self) -> Result<&Query<ast::Crate>> {
        self.parse.compute(|| {
            passes::parse(self.session(), &self.compiler.input).map_err(|mut parse_error| {
                parse_error.emit();
                ErrorReported
            })
        })
    }

    pub fn register_plugins(&self) -> Result<&Query<(ast::Crate, Lrc<LintStore>)>> {
        self.register_plugins.compute(|| {
            let crate_name = self.crate_name()?.peek().clone();
            let krate = self.parse()?.take();

            let empty: &(dyn Fn(&Session, &mut LintStore) + Sync + Send) = &|_, _| {};
            let result = passes::register_plugins(
                self.session(),
                &*self.codegen_backend().metadata_loader(),
                self.compiler.register_lints.as_deref().unwrap_or_else(|| empty),
                krate,
                &crate_name,
            )?;

            // Compute the dependency graph (in the background). We want to do
            // this as early as possible, to give the DepGraph maximum time to
            // load before dep_graph() is called, but it also can't happen
            // until after rustc_incremental::prepare_session_directory() is
            // called, which happens within passes::register_plugins().
            self.dep_graph_future().ok();

            Ok(result)
        })
    }

    pub fn crate_name(&self) -> Result<&Query<String>> {
        self.crate_name.compute(|| {
            Ok({
                let parse_result = self.parse()?;
                let krate = parse_result.peek();
                // parse `#[crate_name]` even if `--crate-name` was passed, to make sure it matches.
                find_crate_name(self.session(), &krate.attrs, &self.compiler.input)
            })
        })
    }

    pub fn expansion(
        &self,
    ) -> Result<&Query<(ast::Crate, Steal<Rc<RefCell<BoxedResolver>>>, Lrc<LintStore>)>> {
        tracing::trace!("expansion");
        self.expansion.compute(|| {
            let crate_name = self.crate_name()?.peek().clone();
            let (krate, lint_store) = self.register_plugins()?.take();
            let _timer = self.session().timer("configure_and_expand");
            let sess = self.session();
            let mut resolver = passes::create_resolver(
                sess.clone(),
                self.codegen_backend().metadata_loader(),
                &krate,
                &crate_name,
            );
            let krate = resolver.access(|resolver| {
                passes::configure_and_expand(&sess, &lint_store, krate, &crate_name, resolver)
            })?;
            Ok((krate, Steal::new(Rc::new(RefCell::new(resolver))), lint_store))
        })
    }

    fn dep_graph(&self) -> Result<&Query<DepGraph>> {
        self.dep_graph.compute(|| {
            let sess = self.session();
            let future_opt = self.dep_graph_future()?.take();
            let dep_graph = future_opt
                .and_then(|future| {
                    let (prev_graph, prev_work_products) =
                        sess.time("blocked_on_dep_graph_loading", || future.open().open(sess));

                    rustc_incremental::build_dep_graph(sess, prev_graph, prev_work_products)
                })
                .unwrap_or_else(DepGraph::new_disabled);
            Ok(dep_graph)
        })
    }

    pub fn prepare_outputs(&self) -> Result<&Query<OutputFilenames>> {
        self.prepare_outputs.compute(|| {
            let expansion_result = self.expansion()?;
            let (krate, boxed_resolver, _) = &*expansion_result.peek();
            let crate_name = self.crate_name()?.peek();
            passes::prepare_outputs(
                self.session(),
                self.compiler,
                &krate,
                &boxed_resolver,
                &crate_name,
            )
        })
    }

    pub fn global_ctxt(&'tcx self) -> Result<&Query<QueryContext<'tcx>>> {
        self.global_ctxt.compute(|| {
            let crate_name = self.crate_name()?.peek().clone();
            let outputs = self.prepare_outputs()?.peek().clone();
            let (ref krate, ref resolver, ref lint_store) = &*self.expansion()?.peek();
            let resolver = resolver.steal();
            let dep_graph = self.dep_graph()?.peek().clone();
            let krate = resolver.borrow_mut().access(|resolver| {
                Ok(passes::lower_to_hir(
                    self.session(),
                    lint_store,
                    resolver,
                    &dep_graph,
                    &krate,
                    &self.hir_arena,
                ))
            })?;
            let krate = self.hir_arena.alloc(krate);
            let resolver_outputs = Steal::new(BoxedResolver::to_resolver_outputs(resolver));
            Ok(passes::create_global_ctxt(
                self.compiler,
                lint_store.clone(),
                krate,
                dep_graph,
                resolver_outputs.steal(),
                outputs,
                &crate_name,
                &self.queries,
                &self.gcx,
                &self.arena,
            ))
        })
    }

    pub fn ongoing_codegen(&'tcx self) -> Result<&Query<Box<dyn Any>>> {
        self.ongoing_codegen.compute(|| {
            let outputs = self.prepare_outputs()?;
            self.global_ctxt()?.peek_mut().enter(|tcx| {
                tcx.analysis(()).ok();

                // Don't do code generation if there were any errors
                self.session().compile_status()?;

                // Hook for UI tests.
                Self::check_for_rustc_errors_attr(tcx);

                Ok(passes::start_codegen(&***self.codegen_backend(), tcx, &*outputs.peek()))
            })
        })
    }

    /// Check for the `#[rustc_error]` annotation, which forces an error in codegen. This is used
    /// to write UI tests that actually test that compilation succeeds without reporting
    /// an error.
    fn check_for_rustc_errors_attr(tcx: TyCtxt<'_>) {
        let def_id = match tcx.entry_fn(()) {
            Some((def_id, _)) => def_id,
            _ => return,
        };

        let attrs = &*tcx.get_attrs(def_id);
        let attrs = attrs.iter().filter(|attr| tcx.sess.check_name(attr, sym::rustc_error));
        for attr in attrs {
            match attr.meta_item_list() {
                // Check if there is a `#[rustc_error(delay_span_bug_from_inside_query)]`.
                Some(list)
                    if list.iter().any(|list_item| {
                        matches!(
                            list_item.ident().map(|i| i.name),
                            Some(sym::delay_span_bug_from_inside_query)
                        )
                    }) =>
                {
                    tcx.ensure().trigger_delay_span_bug(def_id);
                }

                // Bare `#[rustc_error]`.
                None => {
                    tcx.sess.span_fatal(
                        tcx.def_span(def_id),
                        "fatal error triggered by #[rustc_error]",
                    );
                }

                // Some other attribute.
                Some(_) => {
                    tcx.sess.span_warn(
                        tcx.def_span(def_id),
                        "unexpected annotation used with `#[rustc_error(...)]!",
                    );
                }
            }
        }
    }

    pub fn linker(&'tcx self) -> Result<Linker> {
        let sess = self.session().clone();
        let codegen_backend = self.codegen_backend().clone();

        let dep_graph = self.dep_graph()?.peek().clone();
        let prepare_outputs = self.prepare_outputs()?.take();
        let crate_hash = self.global_ctxt()?.peek_mut().enter(|tcx| tcx.crate_hash(LOCAL_CRATE));
        let ongoing_codegen = self.ongoing_codegen()?.take();

        Ok(Linker {
            sess,
            codegen_backend,

            dep_graph,
            prepare_outputs,
            crate_hash,
            ongoing_codegen,
        })
    }
}

pub struct Linker {
    // compilation inputs
    sess: Lrc<Session>,
    codegen_backend: Lrc<Box<dyn CodegenBackend>>,

    // compilation outputs
    dep_graph: DepGraph,
    prepare_outputs: OutputFilenames,
    crate_hash: Svh,
    ongoing_codegen: Box<dyn Any>,
}

impl Linker {
    pub fn link(self) -> Result<()> {
        let (codegen_results, work_products) =
            self.codegen_backend.join_codegen(self.ongoing_codegen, &self.sess)?;

        self.sess.compile_status()?;

        let sess = &self.sess;
        let dep_graph = self.dep_graph;
        sess.time("serialize_work_products", || {
            rustc_incremental::save_work_product_index(&sess, &dep_graph, work_products)
        });

        let prof = self.sess.prof.clone();
        prof.generic_activity("drop_dep_graph").run(move || drop(dep_graph));

        // Now that we won't touch anything in the incremental compilation directory
        // any more, we can finalize it (which involves renaming it)
        rustc_incremental::finalize_session_directory(&self.sess, self.crate_hash);

        if !self
            .sess
            .opts
            .output_types
            .keys()
            .any(|&i| i == OutputType::Exe || i == OutputType::Metadata)
        {
            return Ok(());
        }

        if sess.opts.debugging_opts.no_link {
            // FIXME: use a binary format to encode the `.rlink` file
            let rlink_data = json::encode(&codegen_results).map_err(|err| {
                sess.fatal(&format!("failed to encode rlink: {}", err));
            })?;
            let rlink_file = self.prepare_outputs.with_extension(config::RLINK_EXT);
            std::fs::write(&rlink_file, rlink_data).map_err(|err| {
                sess.fatal(&format!("failed to write file {}: {}", rlink_file.display(), err));
            })?;
            return Ok(());
        }

        let _timer = sess.prof.verbose_generic_activity("link_crate");
        self.codegen_backend.link(&self.sess, codegen_results, &self.prepare_outputs)
    }
}

impl Compiler {
    pub fn enter<F, T>(&self, f: F) -> T
    where
        F: for<'tcx> FnOnce(&'tcx Queries<'tcx>) -> T,
    {
        let mut _timer = None;
        let queries = Queries::new(&self);
        let ret = f(&queries);

        // NOTE: intentionally does not compute the global context if it hasn't been built yet,
        // since that likely means there was a parse error.
        if let Some(Ok(gcx)) = &mut *queries.global_ctxt.result.borrow_mut() {
            // We assume that no queries are run past here. If there are new queries
            // after this point, they'll show up as "<unknown>" in self-profiling data.
            {
                let _prof_timer =
                    queries.session().prof.generic_activity("self_profile_alloc_query_strings");
                gcx.enter(rustc_query_impl::alloc_self_profile_query_strings);
            }

            if self.session().opts.debugging_opts.query_stats {
                gcx.enter(rustc_query_impl::print_stats);
            }

            self.session()
                .time("serialize_dep_graph", || gcx.enter(rustc_incremental::save_dep_graph));
        }

        _timer = Some(self.session().timer("free_global_ctxt"));

        ret
    }
}
