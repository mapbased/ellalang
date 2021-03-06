//! Variable resolution pass.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use ella_parser::ast::{Expr, ExprKind, Stmt, StmtKind};
use ella_parser::lexer::Token;
use ella_parser::visitor::{walk_expr, Visitor};
use ella_source::{Source, SyntaxError};
use ella_value::BuiltinVars;

/// Result of running [`Resolver`] pass.
/// See [`Resolver::resolve_result`].
#[derive(Debug, Clone)]
pub struct ResolveResult {
    symbol_table: SymbolTable,
    resolved_symbol_table: ResolvedSymbolTable,
    accessible_symbols: Vec<Rc<RefCell<Symbol>>>
}

impl ResolveResult {
    /// Lookup a [`Stmt`] (by reference) to get variable resolution metadata.
    pub fn lookup_declaration(&self, stmt: &Stmt) -> Option<&Rc<RefCell<Symbol>>> {
        self.symbol_table.get(&(stmt as *const Stmt))
    }

    /// Lookup a [`Expr`] (by reference) to get variable resolution metadata.
    pub fn lookup_identifier(&self, expr: &Expr) -> Option<&ResolvedSymbol> {
        self.resolved_symbol_table.get(&(expr as *const Expr))
    }

    /// Lookup an identifier in the current `accessible_symbols` list.
    pub fn lookup_in_accessible_symbols(&self, ident: &str) -> Option<&Rc<RefCell<Symbol>>> {
        for symbol in self.accessible_symbols.iter().rev() {
            if symbol.borrow().ident == ident {
                return Some(symbol);
            }
        }
        None
    }
}

/// Represents a symbol (created using `let`, `fn` declaration statement or lambda expression).
#[derive(Debug, PartialEq)]
pub struct Symbol {
    ident: String,
    scope_depth: u32,
    pub is_captured: bool,
    pub upvalues: Vec<ResolvedUpValue>,
    pub stmt: *const Stmt,
}

/// Represents a resolved upvalue (captured variable).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedUpValue {
    pub is_local: bool,
    pub index: i32,
}

/// Represents a resolved symbol (identifier or function call expressions).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSymbol {
    /// The offset relative to the current function's offset (`current_func_offset`).
    pub offset: i32,
    /// Optimization to emit `ldglobal` and `stglobal` instructions.
    pub is_global: bool,
    pub is_upvalue: bool,
    pub symbol: Rc<RefCell<Symbol>>,
}

/// A [`HashMap`] mapping [`Stmt`]s to [`Symbol`]s.
pub type SymbolTable = HashMap<*const Stmt, Rc<RefCell<Symbol>>>;
/// A [`HashMap`] mapping [`Expr`] to [`ResolvedSymbol`]s.
pub type ResolvedSymbolTable = HashMap<*const Expr, ResolvedSymbol>;

/// Variable resolution pass.
pub struct Resolver<'a> {
    /// A [`HashMap`] mapping all declaration [`Stmt`]s to [`Symbol`]s.
    symbol_table: SymbolTable,
    /// A [`HashMap`] mapping all [`Expr::Identifier`]s to [`ResolvedSymbol`]s.
    resolved_symbol_table: ResolvedSymbolTable,
    /// A [`Vec`] of symbols that are currently in (lexical) scope.
    accessible_symbols: Vec<Rc<RefCell<Symbol>>>,
    /// A stack of current function scope depths. `0` is global scope.
    function_scope_depths: Vec<u32>,
    /// Every time a new function scope is created, `current_func_offset` should be set to `self.resolved_symbols.len()`.
    /// When exiting a function scope, the value should be reverted to previous value.
    current_func_offset: i32,
    /// A stack of current function upvalues.
    function_upvalues: Vec<Vec<ResolvedUpValue>>,
    source: Source<'a>,
}

impl<'a> Resolver<'a> {
    /// Create a new empty `Resolver`.
    pub fn new(source: Source<'a>) -> Self {
        Self {
            symbol_table: SymbolTable::new(),
            resolved_symbol_table: ResolvedSymbolTable::new(),
            accessible_symbols: Vec::new(),
            function_scope_depths: vec![0],
            current_func_offset: 0,
            function_upvalues: vec![Vec::new()],
            source,
        }
    }

    /// Create a new `Resolver` with existing accessible symbols.
    /// This method is used to implement REPL functionality (for restoring global variables).
    /// See [`Self::into_resolve_result`].
    pub fn new_with_existing_resolve_result(
        source: Source<'a>,
        resolve_result: ResolveResult,
    ) -> Self {
        Self {
            symbol_table: resolve_result.symbol_table,
            resolved_symbol_table: resolve_result.resolved_symbol_table,
            accessible_symbols: resolve_result.accessible_symbols,
            ..Self::new(source)
        }
    }

    /// Creates a [`ResolveResult`]. This method is used to implement REPL functionality (for restoring global variables).
    /// See [`Self::new_with_existing_resolve_result`].
    pub fn into_resolve_result(self) -> ResolveResult {
        ResolveResult {
            symbol_table: self.symbol_table,
            resolved_symbol_table: self.resolved_symbol_table,
            accessible_symbols: self.accessible_symbols,
        }
    }

    /// Enter a scope.
    fn enter_scope(&mut self) {
        *self.function_scope_depths.last_mut().unwrap() += 1;
    }

    /// Exit a scope. Removes all declarations introduced in previous scope.
    fn exit_scope(&mut self) {
        *self.function_scope_depths.last_mut().unwrap() -= 1;

        // Remove all symbols in current scope.
        self.accessible_symbols = self
            .accessible_symbols
            .iter()
            .filter(|symbol| {
                symbol.borrow().scope_depth <= *self.function_scope_depths.last().unwrap()
            })
            .cloned()
            .collect();
    }

    /// Adds a symbol to `self.accessible_symbols` and `self.symbol_table`.
    fn add_symbol(&mut self, ident: String, stmt: Option<&Stmt>) {
        let symbol = Rc::new(RefCell::new(Symbol {
            ident,
            scope_depth: *self.function_scope_depths.last().unwrap(),
            is_captured: false, // not captured by default
            upvalues: Vec::new(),
            stmt: if let Some(stmt) = stmt {
                stmt as *const Stmt
            } else {
                std::ptr::null()
            },
        }));
        self.accessible_symbols.push(Rc::clone(&symbol));
        if let Some(stmt) = stmt {
            self.symbol_table.insert(stmt as *const Stmt, symbol);
        }
    }

    /// Returns the function scope depth of the specified `scope_depth`.
    fn find_function_scope_depth(&self, scope_depth: u32) -> usize {
        for (i, function_scope_depth) in self.function_scope_depths.iter().enumerate().rev() {
            if *function_scope_depth < scope_depth {
                return i + 1;
            }
        }
        return 0;
    }

    /// Returns `true` if both scope depths are in the same function (e.g. using block statements). Returns `false` otherwise.
    fn in_same_function_scope(&self, first: u32, second: u32) -> bool {
        self.find_function_scope_depth(first) == self.find_function_scope_depth(second)
    }

    /// Returns a `Some((usize, Rc<RefCell<Symbol>>))` or `None` if cannot be resolved.
    /// The `usize` is the offset of the variable.
    ///
    /// # Params
    /// * `ident` - The identifier to resolve.
    /// * `span` - The span of the expression to resolve. This is used for error reporting in case the variable could not be resolved.
    fn resolve_symbol(
        &mut self,
        ident: &str,
        span: Range<usize>,
    ) -> Option<(usize, Rc<RefCell<Symbol>>)> {
        for (i, symbol) in self.accessible_symbols.iter().enumerate().rev() {
            if symbol.borrow().ident == ident {
                if self.find_function_scope_depth(symbol.borrow().scope_depth) == 0 {
                    return Some((i, symbol.clone()));
                } else if self.in_same_function_scope(
                    symbol.borrow().scope_depth,
                    *self.function_scope_depths.last().unwrap(),
                ) {
                    return Some((i - self.current_func_offset as usize, symbol.clone()));
                } else {
                    // capture outer variable
                    symbol.borrow_mut().is_captured = true;

                    // thread upvalue in enclosing functions
                    let mut prev_upvalue_index = 0;
                    for scope_depth in self.find_function_scope_depth(symbol.borrow().scope_depth)
                        + 1
                        ..=self
                            .find_function_scope_depth(*self.function_scope_depths.last().unwrap())
                    {
                        let is_local = scope_depth == symbol.borrow().scope_depth as usize + 1;
                        self.function_upvalues[scope_depth as usize].push(ResolvedUpValue {
                            is_local,
                            index: if is_local {
                                i as i32
                            } else {
                                prev_upvalue_index as i32
                            },
                        });

                        prev_upvalue_index = self.function_upvalues[scope_depth as usize].len() - 1;
                    }

                    return Some((
                        (self.function_upvalues.last().unwrap().len() - 1) as usize,
                        symbol.clone(),
                    ));
                }
            }
        }
        self.source.errors.add_error(
            SyntaxError::new(format!("cannot resolve symbol \"{}\"", ident), span)
                .with_help(format!("make sure symbol \"{}\" is in scope", ident)),
        );
        None
    }

    /// Resolve a top-level function [`Stmt`]. This should be used over calling `visit_stmt`.
    pub fn resolve_top_level(&mut self, func: &'a Stmt) {
        match &func.kind {
            StmtKind::FnDeclaration { body, .. } => {
                for stmt in body {
                    self.visit_stmt(stmt);
                }
            }
            _ => panic!("func is not a StmtKind::FnDeclaration"),
        }
    }

    /// Resolve builtin variables.
    pub fn resolve_builtin_vars(&mut self, builtin_vars: &BuiltinVars) {
        for (ident, _value, _ty) in &builtin_vars.values {
            self.add_symbol(ident.clone(), None);
        }
    }
}

impl<'a> Visitor<'a> for Resolver<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let ExprKind::Lambda { .. } = &expr.kind {
            // custom walking logic for lambda
        } else {
            walk_expr(self, expr);
        }

        match &expr.kind {
            ExprKind::Identifier(ident) => {
                let symbol = self.resolve_symbol(ident, expr.span.clone());
                if let Some((offset, symbol)) = symbol {
                    self.resolved_symbol_table.insert(
                        expr as *const Expr,
                        ResolvedSymbol {
                            offset: offset as i32,
                            is_global: self.find_function_scope_depth(symbol.borrow().scope_depth)
                                == 0,
                            is_upvalue: self.find_function_scope_depth(
                                *self.function_scope_depths.last().unwrap(),
                            ) > self
                                .find_function_scope_depth(symbol.borrow().scope_depth),
                            symbol: symbol.clone(),
                        },
                    );
                }
            }
            ExprKind::Binary {
                lhs,
                op: Token::Equals,
                rhs: _,
            } => {
                // make sure lhs is an identifier
                match &lhs.kind {
                    ExprKind::Identifier(_ident) => {}
                    _ => self.source.errors.add_error(
                        SyntaxError::new("invalid left-hand side of assignment", lhs.span.clone())
                            .with_help("left-hand side of an assignment must be an identifier"),
                    ),
                };
            }
            ExprKind::Lambda {
                inner_stmt,
                params,
                body,
            } => {
                let ident = "lambda".to_string();
                let old_func_offset = self.current_func_offset;

                self.current_func_offset = self.accessible_symbols.len() as i32;
                self.function_upvalues.push(Vec::new());
                self.function_scope_depths
                    .push(*self.function_scope_depths.last().unwrap());

                self.enter_scope();
                // add arguments
                for param in params {
                    self.visit_stmt(param);
                }

                for stmt in body {
                    self.visit_stmt(stmt);
                }
                self.exit_scope();

                // patch self.symbol_table with upvalues
                self.function_scope_depths.pop();
                self.symbol_table.insert(
                    inner_stmt.as_ref() as *const Stmt,
                    Rc::new(RefCell::new(Symbol {
                        ident,
                        is_captured: false,
                        scope_depth: *self.function_scope_depths.last().unwrap(),
                        upvalues: self.function_upvalues.pop().unwrap(),
                        stmt: inner_stmt.as_ref() as *const Stmt,
                    })),
                );

                self.current_func_offset = old_func_offset;
            }
            _ => {}
        }
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        // Do not use default walking logic.

        match &stmt.kind {
            StmtKind::LetDeclaration {
                ident,
                initializer,
                ty: _,
            } => {
                self.visit_expr(initializer);
                self.add_symbol(ident.clone(), Some(stmt));
            }
            StmtKind::FnParam {
                ident
            } => {
                self.add_symbol(ident.clone(), Some(stmt));
            }
            StmtKind::FnDeclaration {
                ident,
                params,
                body,
            } => {
                self.add_symbol(ident.clone(), Some(stmt)); // Add symbol first to allow for recursion.

                let old_func_offset = self.current_func_offset;

                self.current_func_offset = self.accessible_symbols.len() as i32;
                self.function_upvalues.push(Vec::new());
                self.function_scope_depths
                    .push(*self.function_scope_depths.last().unwrap());

                self.enter_scope();
                // add arguments
                for param in params {
                    self.visit_stmt(param);
                }

                for stmt in body {
                    self.visit_stmt(stmt);
                }
                self.exit_scope();

                // patch self.symbol_table with upvalues
                self.symbol_table
                    .get(&(stmt as *const Stmt))
                    .unwrap()
                    .borrow_mut()
                    .upvalues = self.function_upvalues.pop().unwrap();
                self.function_scope_depths.pop();

                self.current_func_offset = old_func_offset;
            }
            StmtKind::Block(body) => {
                self.enter_scope();
                for stmt in body {
                    self.visit_stmt(stmt);
                }
                self.exit_scope();
            }
            StmtKind::IfElseStmt {
                condition,
                if_block,
                else_block,
            } => {
                self.visit_expr(condition);
                self.enter_scope();
                for stmt in if_block {
                    self.visit_stmt(stmt);
                }
                self.exit_scope();
                if let Some(else_block) = else_block {
                    self.enter_scope();
                    for stmt in else_block {
                        self.visit_stmt(stmt);
                    }
                    self.exit_scope();
                }
            }
            StmtKind::WhileStmt { condition, body } => {
                self.visit_expr(condition);
                self.enter_scope();
                for stmt in body {
                    self.visit_stmt(stmt);
                }
                self.exit_scope();
            }
            StmtKind::ExprStmt(expr) => self.visit_expr(expr),
            StmtKind::ReturnStmt(expr) => self.visit_expr(expr),
            StmtKind::Lambda => unreachable!(),
            StmtKind::Error => {}
        }
    }
}
