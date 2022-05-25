//! Evaluation of markup into modules.

#[macro_use]
mod array;
#[macro_use]
mod dict;
#[macro_use]
mod value;

mod args;
mod capture;
mod func;
mod machine;
pub mod methods;
pub mod ops;
mod raw;
mod scope;
mod str;

pub use self::str::*;
pub use args::*;
pub use array::*;
pub use capture::*;
pub use dict::*;
pub use func::*;
pub use machine::*;
pub use raw::*;
pub use scope::*;
pub use value::*;

use std::collections::BTreeMap;

use parking_lot::{MappedRwLockWriteGuard, RwLockWriteGuard};
use unicode_segmentation::UnicodeSegmentation;

use crate::diag::{At, StrResult, Trace, Tracepoint, TypResult};
use crate::geom::{Angle, Em, Fraction, Length, Ratio};
use crate::library;
use crate::model::{Content, Pattern, Recipe, StyleEntry, StyleMap};
use crate::source::{SourceId, SourceStore};
use crate::syntax::ast::*;
use crate::syntax::{Span, Spanned};
use crate::util::EcoString;
use crate::Context;

/// Evaluate a source file and return the resulting module.
///
/// Returns either a module containing a scope with top-level bindings and
/// layoutable contents or diagnostics in the form of a vector of error
/// messages with file and span information.
pub fn evaluate(
    ctx: &mut Context,
    id: SourceId,
    mut route: Vec<SourceId>,
) -> TypResult<Module> {
    // Prevent cyclic evaluation.
    if route.contains(&id) {
        let path = ctx.sources.get(id).path().display();
        panic!("Tried to cyclicly evaluate {}", path);
    }

    // Check whether the module was already evaluated.
    if let Some(module) = ctx.modules.get(&id) {
        if module.valid(&ctx.sources) {
            return Ok(module.clone());
        } else {
            ctx.modules.remove(&id);
        }
    }

    route.push(id);

    // Parse the file.
    let source = ctx.sources.get(id);
    let ast = source.ast()?;

    // Save the old dependencies.
    let prev_deps = std::mem::replace(&mut ctx.deps, vec![(id, source.rev())]);

    // Evaluate the module.
    let std = ctx.config.std.clone();
    let scopes = Scopes::new(Some(&std));
    let mut vm = Machine::new(ctx, route, scopes);
    let result = ast.eval(&mut vm);
    let scope = vm.scopes.top;
    let flow = vm.flow;

    // Restore the and dependencies.
    let deps = std::mem::replace(&mut ctx.deps, prev_deps);

    // Handle control flow.
    if let Some(flow) = flow {
        return Err(flow.forbidden());
    }

    // Assemble the module.
    let module = Module { scope, content: result?, deps };

    // Save the evaluated module.
    ctx.modules.insert(id, module.clone());

    Ok(module)
}

/// An evaluated module, ready for importing or layouting.
#[derive(Debug, Clone)]
pub struct Module {
    /// The top-level definitions that were bound in this module.
    pub scope: Scope,
    /// The module's layoutable contents.
    pub content: Content,
    /// The source file revisions this module depends on.
    pub deps: Vec<(SourceId, usize)>,
}

impl Module {
    /// Whether the module is still valid for the given sources.
    pub fn valid(&self, sources: &SourceStore) -> bool {
        self.deps.iter().all(|&(id, rev)| rev == sources.get(id).rev())
    }
}

/// Evaluate an expression.
pub trait Eval {
    /// The output of evaluating the expression.
    type Output;

    /// Evaluate the expression to the output value.
    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output>;
}

impl Eval for Markup {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        eval_markup(vm, &mut self.nodes())
    }
}

/// Evaluate a stream of markup nodes.
fn eval_markup(
    vm: &mut Machine,
    nodes: &mut impl Iterator<Item = MarkupNode>,
) -> TypResult<Content> {
    let flow = vm.flow.take();
    let mut seq = Vec::with_capacity(nodes.size_hint().1.unwrap_or_default());

    while let Some(node) = nodes.next() {
        seq.push(match node {
            MarkupNode::Expr(Expr::Set(set)) => {
                let styles = set.eval(vm)?;
                if vm.flow.is_some() {
                    break;
                }

                eval_markup(vm, nodes)?.styled_with_map(styles)
            }
            MarkupNode::Expr(Expr::Show(show)) => {
                let recipe = show.eval(vm)?;
                if vm.flow.is_some() {
                    break;
                }

                eval_markup(vm, nodes)?
                    .styled_with_entry(StyleEntry::Recipe(recipe).into())
            }
            MarkupNode::Expr(Expr::Wrap(wrap)) => {
                let tail = eval_markup(vm, nodes)?;
                vm.scopes.top.def_mut(wrap.binding().take(), tail);
                wrap.body().eval(vm)?.display()
            }

            _ => node.eval(vm)?,
        });

        if vm.flow.is_some() {
            break;
        }
    }

    if flow.is_some() {
        vm.flow = flow;
    }

    Ok(Content::sequence(seq))
}

impl Eval for MarkupNode {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        Ok(match self {
            Self::Space => Content::Space,
            Self::Parbreak => Content::Parbreak,
            &Self::Linebreak { justified } => Content::Linebreak { justified },
            Self::Text(text) => Content::Text(text.clone()),
            &Self::Quote { double } => Content::Quote { double },
            Self::Strong(strong) => strong.eval(vm)?,
            Self::Emph(emph) => emph.eval(vm)?,
            Self::Raw(raw) => raw.eval(vm)?,
            Self::Math(math) => math.eval(vm)?,
            Self::Heading(heading) => heading.eval(vm)?,
            Self::List(list) => list.eval(vm)?,
            Self::Enum(enum_) => enum_.eval(vm)?,
            Self::Expr(expr) => expr.eval(vm)?.display(),
        })
    }
}

impl Eval for StrongNode {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        Ok(Content::show(library::text::StrongNode(
            self.body().eval(vm)?,
        )))
    }
}

impl Eval for EmphNode {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        Ok(Content::show(library::text::EmphNode(
            self.body().eval(vm)?,
        )))
    }
}

impl Eval for RawNode {
    type Output = Content;

    fn eval(&self, _: &mut Machine) -> TypResult<Self::Output> {
        let content = Content::show(library::text::RawNode {
            text: self.text.clone(),
            block: self.block,
        });
        Ok(match self.lang {
            Some(_) => content.styled(library::text::RawNode::LANG, self.lang.clone()),
            None => content,
        })
    }
}

impl Eval for Spanned<MathNode> {
    type Output = Content;

    fn eval(&self, _: &mut Machine) -> TypResult<Self::Output> {
        Ok(Content::show(library::math::MathNode {
            formula: self.clone().map(|math| math.formula),
            display: self.v.display,
        }))
    }
}

impl Eval for HeadingNode {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        Ok(Content::show(library::structure::HeadingNode {
            body: self.body().eval(vm)?,
            level: self.level(),
        }))
    }
}

impl Eval for ListNode {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        Ok(Content::Item(library::structure::ListItem {
            kind: library::structure::UNORDERED,
            number: None,
            body: Box::new(self.body().eval(vm)?),
        }))
    }
}

impl Eval for EnumNode {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        Ok(Content::Item(library::structure::ListItem {
            kind: library::structure::ORDERED,
            number: self.number(),
            body: Box::new(self.body().eval(vm)?),
        }))
    }
}

impl Eval for Expr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let forbidden = |name| {
            error!(
                self.span(),
                "{} is only allowed directly in code and content blocks", name
            )
        };

        match self {
            Self::Lit(v) => v.eval(vm),
            Self::Ident(v) => v.eval(vm),
            Self::Code(v) => v.eval(vm),
            Self::Content(v) => v.eval(vm).map(Value::Content),
            Self::Array(v) => v.eval(vm).map(Value::Array),
            Self::Dict(v) => v.eval(vm).map(Value::Dict),
            Self::Group(v) => v.eval(vm),
            Self::FieldAccess(v) => v.eval(vm),
            Self::FuncCall(v) => v.eval(vm),
            Self::MethodCall(v) => v.eval(vm),
            Self::Closure(v) => v.eval(vm),
            Self::Unary(v) => v.eval(vm),
            Self::Binary(v) => v.eval(vm),
            Self::Let(v) => v.eval(vm),
            Self::Set(_) => Err(forbidden("set")),
            Self::Show(_) => Err(forbidden("show")),
            Self::Wrap(_) => Err(forbidden("wrap")),
            Self::If(v) => v.eval(vm),
            Self::While(v) => v.eval(vm),
            Self::For(v) => v.eval(vm),
            Self::Import(v) => v.eval(vm),
            Self::Include(v) => v.eval(vm).map(Value::Content),
            Self::Break(v) => v.eval(vm),
            Self::Continue(v) => v.eval(vm),
            Self::Return(v) => v.eval(vm),
        }
    }
}

impl Eval for Lit {
    type Output = Value;

    fn eval(&self, _: &mut Machine) -> TypResult<Self::Output> {
        Ok(match self.kind() {
            LitKind::None => Value::None,
            LitKind::Auto => Value::Auto,
            LitKind::Bool(v) => Value::Bool(v),
            LitKind::Int(v) => Value::Int(v),
            LitKind::Float(v) => Value::Float(v),
            LitKind::Numeric(v, unit) => match unit {
                Unit::Length(unit) => Length::with_unit(v, unit).into(),
                Unit::Angle(unit) => Angle::with_unit(v, unit).into(),
                Unit::Em => Em::new(v).into(),
                Unit::Fr => Fraction::new(v).into(),
                Unit::Percent => Ratio::new(v / 100.0).into(),
            },
            LitKind::Str(ref v) => Value::Str(v.clone()),
        })
    }
}

impl Eval for Ident {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        match vm.scopes.get(self) {
            Some(slot) => Ok(slot.read().clone()),
            None => bail!(self.span(), "unknown variable"),
        }
    }
}

impl Eval for CodeBlock {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        vm.scopes.enter();
        let output = eval_code(vm, &mut self.exprs())?;
        vm.scopes.exit();
        Ok(output)
    }
}

/// Evaluate a stream of expressions.
fn eval_code(
    vm: &mut Machine,
    exprs: &mut impl Iterator<Item = Expr>,
) -> TypResult<Value> {
    let flow = vm.flow.take();
    let mut output = Value::None;

    while let Some(expr) = exprs.next() {
        let span = expr.span();
        let value = match expr {
            Expr::Set(set) => {
                let styles = set.eval(vm)?;
                if vm.flow.is_some() {
                    break;
                }

                let tail = eval_code(vm, exprs)?.display();
                Value::Content(tail.styled_with_map(styles))
            }
            Expr::Show(show) => {
                let recipe = show.eval(vm)?;
                let entry = StyleEntry::Recipe(recipe).into();
                if vm.flow.is_some() {
                    break;
                }

                let tail = eval_code(vm, exprs)?.display();
                Value::Content(tail.styled_with_entry(entry))
            }
            Expr::Wrap(wrap) => {
                let tail = eval_code(vm, exprs)?;
                vm.scopes.top.def_mut(wrap.binding().take(), tail);
                wrap.body().eval(vm)?
            }

            _ => expr.eval(vm)?,
        };

        output = ops::join(output, value).at(span)?;

        if vm.flow.is_some() {
            break;
        }
    }

    if flow.is_some() {
        vm.flow = flow;
    }

    Ok(output)
}

impl Eval for ContentBlock {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        vm.scopes.enter();
        let content = self.body().eval(vm)?;
        vm.scopes.exit();
        Ok(content)
    }
}

impl Eval for GroupExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        self.expr().eval(vm)
    }
}

impl Eval for ArrayExpr {
    type Output = Array;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let items = self.items();

        let mut vec = Vec::with_capacity(items.size_hint().0);
        for item in items {
            match item {
                ArrayItem::Pos(expr) => vec.push(expr.eval(vm)?),
                ArrayItem::Spread(expr) => match expr.eval(vm)? {
                    Value::None => {}
                    Value::Array(array) => vec.extend(array.into_iter()),
                    v => bail!(expr.span(), "cannot spread {} into array", v.type_name()),
                },
            }
        }

        Ok(Array::from_vec(vec))
    }
}

impl Eval for DictExpr {
    type Output = Dict;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let mut map = BTreeMap::new();

        for item in self.items() {
            match item {
                DictItem::Named(named) => {
                    map.insert(named.name().take(), named.expr().eval(vm)?);
                }
                DictItem::Keyed(keyed) => {
                    map.insert(keyed.key(), keyed.expr().eval(vm)?);
                }
                DictItem::Spread(expr) => match expr.eval(vm)? {
                    Value::None => {}
                    Value::Dict(dict) => map.extend(dict.into_iter()),
                    v => bail!(
                        expr.span(),
                        "cannot spread {} into dictionary",
                        v.type_name()
                    ),
                },
            }
        }

        Ok(Dict::from_map(map))
    }
}

impl Eval for UnaryExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let value = self.expr().eval(vm)?;
        let result = match self.op() {
            UnOp::Pos => ops::pos(value),
            UnOp::Neg => ops::neg(value),
            UnOp::Not => ops::not(value),
        };
        Ok(result.at(self.span())?)
    }
}

impl Eval for BinaryExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        match self.op() {
            BinOp::Add => self.apply(vm, ops::add),
            BinOp::Sub => self.apply(vm, ops::sub),
            BinOp::Mul => self.apply(vm, ops::mul),
            BinOp::Div => self.apply(vm, ops::div),
            BinOp::And => self.apply(vm, ops::and),
            BinOp::Or => self.apply(vm, ops::or),
            BinOp::Eq => self.apply(vm, ops::eq),
            BinOp::Neq => self.apply(vm, ops::neq),
            BinOp::Lt => self.apply(vm, ops::lt),
            BinOp::Leq => self.apply(vm, ops::leq),
            BinOp::Gt => self.apply(vm, ops::gt),
            BinOp::Geq => self.apply(vm, ops::geq),
            BinOp::In => self.apply(vm, ops::in_),
            BinOp::NotIn => self.apply(vm, ops::not_in),
            BinOp::Assign => self.assign(vm, |_, b| Ok(b)),
            BinOp::AddAssign => self.assign(vm, ops::add),
            BinOp::SubAssign => self.assign(vm, ops::sub),
            BinOp::MulAssign => self.assign(vm, ops::mul),
            BinOp::DivAssign => self.assign(vm, ops::div),
        }
    }
}

impl BinaryExpr {
    /// Apply a basic binary operation.
    fn apply(
        &self,
        vm: &mut Machine,
        op: fn(Value, Value) -> StrResult<Value>,
    ) -> TypResult<Value> {
        let lhs = self.lhs().eval(vm)?;

        // Short-circuit boolean operations.
        if (self.op() == BinOp::And && lhs == Value::Bool(false))
            || (self.op() == BinOp::Or && lhs == Value::Bool(true))
        {
            return Ok(lhs);
        }

        let rhs = self.rhs().eval(vm)?;
        Ok(op(lhs, rhs).at(self.span())?)
    }

    /// Apply an assignment operation.
    fn assign(
        &self,
        vm: &mut Machine,
        op: fn(Value, Value) -> StrResult<Value>,
    ) -> TypResult<Value> {
        let rhs = self.rhs().eval(vm)?;
        let lhs = self.lhs();
        let mut location = lhs.access(vm)?;
        let lhs = std::mem::take(&mut *location);
        *location = op(lhs, rhs).at(self.span())?;
        Ok(Value::None)
    }
}

impl Eval for FieldAccess {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let object = self.object().eval(vm)?;
        let span = self.field().span();
        let field = self.field().take();

        Ok(match object {
            Value::Dict(dict) => dict.get(&field).at(span)?.clone(),

            Value::Content(Content::Show(_, Some(dict))) => dict
                .get(&field)
                .map_err(|_| format!("unknown field {field:?}"))
                .at(span)?
                .clone(),

            v => bail!(
                self.object().span(),
                "cannot access field on {}",
                v.type_name()
            ),
        })
    }
}

impl Eval for FuncCall {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let callee = self.callee().eval(vm)?;
        let args = self.args().eval(vm)?;

        Ok(match callee {
            Value::Array(array) => array.get(args.into_index()?).at(self.span())?.clone(),
            Value::Dict(dict) => dict.get(&args.into_key()?).at(self.span())?.clone(),
            Value::Func(func) => {
                let point = || Tracepoint::Call(func.name().map(ToString::to_string));
                func.call(vm, args).trace(point, self.span())?
            }

            v => bail!(
                self.callee().span(),
                "expected callable or collection, found {}",
                v.type_name(),
            ),
        })
    }
}

impl Eval for MethodCall {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let span = self.span();
        let method = self.method();
        let point = || Tracepoint::Call(Some(method.to_string()));

        Ok(if methods::is_mutating(&method) {
            let args = self.args().eval(vm)?;
            let mut value = self.receiver().access(vm)?;
            methods::call_mut(&mut value, &method, args, span).trace(point, span)?;
            Value::None
        } else {
            let value = self.receiver().eval(vm)?;
            let args = self.args().eval(vm)?;
            methods::call(vm, value, &method, args, span).trace(point, span)?
        })
    }
}

impl Eval for CallArgs {
    type Output = Args;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let mut items = Vec::new();

        for arg in self.items() {
            let span = arg.span();
            match arg {
                CallArg::Pos(expr) => {
                    items.push(Arg {
                        span,
                        name: None,
                        value: Spanned::new(expr.eval(vm)?, expr.span()),
                    });
                }
                CallArg::Named(named) => {
                    items.push(Arg {
                        span,
                        name: Some(named.name().take()),
                        value: Spanned::new(named.expr().eval(vm)?, named.expr().span()),
                    });
                }
                CallArg::Spread(expr) => match expr.eval(vm)? {
                    Value::None => {}
                    Value::Array(array) => {
                        items.extend(array.into_iter().map(|value| Arg {
                            span,
                            name: None,
                            value: Spanned::new(value, span),
                        }));
                    }
                    Value::Dict(dict) => {
                        items.extend(dict.into_iter().map(|(key, value)| Arg {
                            span,
                            name: Some(key),
                            value: Spanned::new(value, span),
                        }));
                    }
                    Value::Args(args) => items.extend(args.items),
                    v => bail!(expr.span(), "cannot spread {}", v.type_name()),
                },
            }
        }

        Ok(Args { span: self.span(), items })
    }
}

impl Eval for ClosureExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        // The closure's name is defined by its let binding if there's one.
        let name = self.name().map(Ident::take);

        // Collect captured variables.
        let captured = {
            let mut visitor = CapturesVisitor::new(&vm.scopes);
            visitor.visit(self.as_red());
            visitor.finish()
        };

        let mut params = Vec::new();
        let mut sink = None;

        // Collect parameters and an optional sink parameter.
        for param in self.params() {
            match param {
                ClosureParam::Pos(name) => {
                    params.push((name.take(), None));
                }
                ClosureParam::Named(named) => {
                    params.push((named.name().take(), Some(named.expr().eval(vm)?)));
                }
                ClosureParam::Sink(name) => {
                    if sink.is_some() {
                        bail!(name.span(), "only one argument sink is allowed");
                    }
                    sink = Some(name.take());
                }
            }
        }

        // Define the actual function.
        Ok(Value::Func(Func::from_closure(Closure {
            location: vm.route.last().copied(),
            name,
            captured,
            params,
            sink,
            body: self.body(),
        })))
    }
}

impl Eval for LetExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let value = match self.init() {
            Some(expr) => expr.eval(vm)?,
            None => Value::None,
        };
        vm.scopes.top.def_mut(self.binding().take(), value);
        Ok(Value::None)
    }
}

impl Eval for SetExpr {
    type Output = StyleMap;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let target = self.target();
        let target = target.eval(vm)?.cast::<Func>().at(target.span())?;
        let args = self.args().eval(vm)?;
        Ok(target.set(args)?)
    }
}

impl Eval for ShowExpr {
    type Output = Recipe;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        // Evaluate the target function.
        let pattern = self.pattern();
        let pattern = pattern.eval(vm)?.cast::<Pattern>().at(pattern.span())?;

        // Collect captured variables.
        let captured = {
            let mut visitor = CapturesVisitor::new(&vm.scopes);
            visitor.visit(self.as_red());
            visitor.finish()
        };

        // Define parameters.
        let mut params = vec![];
        if let Some(binding) = self.binding() {
            params.push((binding.take(), None));
        }

        // Define the recipe function.
        let body = self.body();
        let span = body.span();
        let func = Func::from_closure(Closure {
            location: vm.route.last().copied(),
            name: None,
            captured,
            params,
            sink: None,
            body,
        });

        Ok(Recipe { pattern, func, span })
    }
}

impl Eval for IfExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let condition = self.condition();
        if condition.eval(vm)?.cast::<bool>().at(condition.span())? {
            self.if_body().eval(vm)
        } else if let Some(else_body) = self.else_body() {
            else_body.eval(vm)
        } else {
            Ok(Value::None)
        }
    }
}

impl Eval for WhileExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let flow = vm.flow.take();
        let mut output = Value::None;

        let condition = self.condition();
        while condition.eval(vm)?.cast::<bool>().at(condition.span())? {
            let body = self.body();
            let value = body.eval(vm)?;
            output = ops::join(output, value).at(body.span())?;

            match vm.flow {
                Some(Flow::Break(_)) => {
                    vm.flow = None;
                    break;
                }
                Some(Flow::Continue(_)) => vm.flow = None,
                Some(Flow::Return(..)) => break,
                None => {}
            }
        }

        if flow.is_some() {
            vm.flow = flow;
        }

        Ok(output)
    }
}

impl Eval for ForExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let flow = vm.flow.take();
        let mut output = Value::None;
        vm.scopes.enter();

        macro_rules! iter {
            (for ($($binding:ident => $value:ident),*) in $iter:expr) => {{
                #[allow(unused_parens)]
                for ($($value),*) in $iter {
                    $(vm.scopes.top.def_mut(&$binding, $value);)*

                    let body = self.body();
                    let value = body.eval(vm)?;
                    output = ops::join(output, value).at(body.span())?;

                    match vm.flow {
                        Some(Flow::Break(_)) => {
                            vm.flow = None;
                            break;
                        }
                        Some(Flow::Continue(_)) => vm.flow = None,
                        Some(Flow::Return(..)) => break,
                        None => {}
                    }
                }

            }};
        }

        let iter = self.iter().eval(vm)?;
        let pattern = self.pattern();
        let key = pattern.key().map(Ident::take);
        let value = pattern.value().take();

        match (key, value, iter) {
            (None, v, Value::Str(string)) => {
                iter!(for (v => value) in string.graphemes(true));
            }
            (None, v, Value::Array(array)) => {
                iter!(for (v => value) in array.into_iter());
            }
            (Some(i), v, Value::Array(array)) => {
                iter!(for (i => idx, v => value) in array.into_iter().enumerate());
            }
            (None, v, Value::Dict(dict)) => {
                iter!(for (v => value) in dict.into_iter().map(|p| p.1));
            }
            (Some(k), v, Value::Dict(dict)) => {
                iter!(for (k => key, v => value) in dict.into_iter());
            }
            (None, v, Value::Args(args)) => {
                iter!(for (v => value) in args.items.into_iter()
                    .filter(|arg| arg.name.is_none())
                    .map(|arg| arg.value.v));
            }
            (Some(k), v, Value::Args(args)) => {
                iter!(for (k => key, v => value) in args.items.into_iter()
                    .map(|arg| (arg.name.map_or(Value::None, Value::Str), arg.value.v)));
            }
            (_, _, Value::Str(_)) => {
                bail!(pattern.span(), "mismatched pattern");
            }
            (_, _, iter) => {
                bail!(self.iter().span(), "cannot loop over {}", iter.type_name());
            }
        }

        if flow.is_some() {
            vm.flow = flow;
        }

        vm.scopes.exit();
        Ok(output)
    }
}

impl Eval for ImportExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let span = self.path().span();
        let path = self.path().eval(vm)?.cast::<EcoString>().at(span)?;
        let module = import(vm, &path, span)?;

        match self.imports() {
            Imports::Wildcard => {
                for (var, slot) in module.scope.iter() {
                    vm.scopes.top.def_mut(var, slot.read().clone());
                }
            }
            Imports::Items(idents) => {
                for ident in idents {
                    if let Some(slot) = module.scope.get(&ident) {
                        vm.scopes.top.def_mut(ident.take(), slot.read().clone());
                    } else {
                        bail!(ident.span(), "unresolved import");
                    }
                }
            }
        }

        Ok(Value::None)
    }
}

impl Eval for IncludeExpr {
    type Output = Content;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let span = self.path().span();
        let path = self.path().eval(vm)?.cast::<EcoString>().at(span)?;
        let module = import(vm, &path, span)?;
        Ok(module.content.clone())
    }
}

/// Process an import of a module relative to the current location.
fn import(vm: &mut Machine, path: &str, span: Span) -> TypResult<Module> {
    // Load the source file.
    let full = vm.locate(&path).at(span)?;
    let id = vm.ctx.sources.load(&full).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => {
            error!(span, "file not found (searched at {})", full.display())
        }
        _ => error!(span, "failed to load source file ({})", err),
    })?;

    // Prevent cyclic importing.
    if vm.route.contains(&id) {
        bail!(span, "cyclic import");
    }

    // Evaluate the file.
    let route = vm.route.clone();
    let module = evaluate(vm.ctx, id, route).trace(|| Tracepoint::Import, span)?;
    vm.ctx.deps.extend(module.deps.iter().cloned());
    Ok(module)
}

impl Eval for BreakExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        if vm.flow.is_none() {
            vm.flow = Some(Flow::Break(self.span()));
        }
        Ok(Value::None)
    }
}

impl Eval for ContinueExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        if vm.flow.is_none() {
            vm.flow = Some(Flow::Continue(self.span()));
        }
        Ok(Value::None)
    }
}

impl Eval for ReturnExpr {
    type Output = Value;

    fn eval(&self, vm: &mut Machine) -> TypResult<Self::Output> {
        let value = self.body().map(|body| body.eval(vm)).transpose()?;
        if vm.flow.is_none() {
            vm.flow = Some(Flow::Return(self.span(), value));
        }
        Ok(Value::None)
    }
}

/// Access an expression mutably.
pub trait Access {
    /// Access the value.
    fn access<'a>(&self, vm: &'a mut Machine) -> TypResult<Location<'a>>;
}

impl Access for Expr {
    fn access<'a>(&self, vm: &'a mut Machine) -> TypResult<Location<'a>> {
        match self {
            Expr::Ident(v) => v.access(vm),
            Expr::FieldAccess(v) => v.access(vm),
            Expr::FuncCall(v) => v.access(vm),
            _ => bail!(self.span(), "cannot mutate a temporary value"),
        }
    }
}

impl Access for Ident {
    fn access<'a>(&self, vm: &'a mut Machine) -> TypResult<Location<'a>> {
        match vm.scopes.get(self) {
            Some(slot) => match slot.try_write() {
                Some(guard) => Ok(RwLockWriteGuard::map(guard, |v| v)),
                None => bail!(self.span(), "cannot mutate a constant"),
            },
            None => bail!(self.span(), "unknown variable"),
        }
    }
}

impl Access for FieldAccess {
    fn access<'a>(&self, vm: &'a mut Machine) -> TypResult<Location<'a>> {
        let guard = self.object().access(vm)?;
        try_map(guard, |value| {
            Ok(match value {
                Value::Dict(dict) => dict.get_mut(self.field().take()),
                v => bail!(
                    self.object().span(),
                    "expected dictionary, found {}",
                    v.type_name(),
                ),
            })
        })
    }
}

impl Access for FuncCall {
    fn access<'a>(&self, vm: &'a mut Machine) -> TypResult<Location<'a>> {
        let args = self.args().eval(vm)?;
        let guard = self.callee().access(vm)?;
        try_map(guard, |value| {
            Ok(match value {
                Value::Array(array) => {
                    array.get_mut(args.into_index()?).at(self.span())?
                }
                Value::Dict(dict) => dict.get_mut(args.into_key()?),
                v => bail!(
                    self.callee().span(),
                    "expected collection, found {}",
                    v.type_name(),
                ),
            })
        })
    }
}

/// A mutable location.
type Location<'a> = MappedRwLockWriteGuard<'a, Value>;

/// Map a reader-writer lock with a function.
fn try_map<F>(location: Location, f: F) -> TypResult<Location>
where
    F: FnOnce(&mut Value) -> TypResult<&mut Value>,
{
    let mut error = None;
    MappedRwLockWriteGuard::try_map(location, |value| match f(value) {
        Ok(value) => Some(value),
        Err(err) => {
            error = Some(err);
            None
        }
    })
    .map_err(|_| error.unwrap())
}
