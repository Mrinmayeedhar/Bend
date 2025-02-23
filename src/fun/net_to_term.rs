use crate::{
  diagnostics::{DiagnosticOrigin, Diagnostics, Severity},
  fun::{term_to_net::Labels, Book, FanKind, Name, Op, Pattern, Tag, Term},
  maybe_grow,
  net::{CtrKind, INet, NodeId, NodeKind, Port, SlotId, ROOT},
};
use std::collections::{BTreeSet, HashMap, HashSet};

use super::Num;

/// Converts an Interaction-INet to a Lambda Calculus term
pub fn net_to_term(
  net: &INet,
  book: &Book,
  labels: &Labels,
  linear: bool,
  diagnostics: &mut Diagnostics,
) -> Term {
  let mut reader = Reader {
    net,
    labels,
    book,
    dup_paths: if linear { None } else { Some(Default::default()) },
    scope: Default::default(),
    seen_fans: Default::default(),
    namegen: Default::default(),
    seen: Default::default(),
    errors: Default::default(),
  };

  let mut term = reader.read_term(net.enter_port(ROOT));

  while let Some(node) = reader.scope.pop_first() {
    let val = reader.read_term(reader.net.enter_port(Port(node, 0)));
    let fst = reader.namegen.decl_name(net, Port(node, 1));
    let snd = reader.namegen.decl_name(net, Port(node, 2));

    let (fan, tag) = match reader.net.node(node).kind {
      NodeKind::Ctr(CtrKind::Tup(lab)) => (FanKind::Tup, reader.labels.tup.to_tag(lab)),
      NodeKind::Ctr(CtrKind::Dup(lab)) => (FanKind::Dup, reader.labels.dup.to_tag(Some(lab))),
      _ => unreachable!(),
    };

    let split = &mut Split { fan, tag, fst, snd, val };

    let uses = term.insert_split(split, usize::MAX).unwrap();
    let result = term.insert_split(split, uses);
    debug_assert_eq!(result, None);
  }

  reader.report_errors(diagnostics);

  let mut unscoped = HashSet::new();
  let mut scope = Vec::new();
  term.collect_unscoped(&mut unscoped, &mut scope);
  term.apply_unscoped(&unscoped);

  term
}

// BTreeSet for consistent readback of dups
type Scope = BTreeSet<NodeId>;

pub struct Reader<'a> {
  pub book: &'a Book,
  pub namegen: NameGen,
  net: &'a INet,
  labels: &'a Labels,
  dup_paths: Option<HashMap<u16, Vec<SlotId>>>,
  /// Store for floating/unscoped terms, like dups and let tups.
  scope: Scope,
  // To avoid reinserting things in the scope.
  seen_fans: Scope,
  seen: HashSet<Port>,
  errors: Vec<ReadbackError>,
}

impl Reader<'_> {
  fn read_term(&mut self, next: Port) -> Term {
    use CtrKind::*;

    maybe_grow(|| {
      if !self.seen.insert(next) && self.dup_paths.is_none() {
        self.error(ReadbackError::Cyclic);
        return Term::Var { nam: Name::new("...") };
      }

      let node = next.node();
      match &self.net.node(node).kind {
        NodeKind::Era => {
          // Only the main port actually exists in an ERA, the aux ports are just an artifact of this representation.
          debug_assert!(next.slot() == 0);
          Term::Era
        }
        // If we're visiting a con node...
        NodeKind::Ctr(CtrKind::Con(lab)) => match next.slot() {
          // If we're visiting a port 0, then it is a tuple or a lambda.
          0 => {
            if self.is_tup(node) {
              // A tuple
              let lft = self.read_term(self.net.enter_port(Port(node, 1)));
              let rgt = self.read_term(self.net.enter_port(Port(node, 2)));
              Term::Fan { fan: FanKind::Tup, tag: self.labels.con.to_tag(*lab), els: vec![lft, rgt] }
            } else {
              // A lambda
              let nam = self.namegen.decl_name(self.net, Port(node, 1));
              let bod = self.read_term(self.net.enter_port(Port(node, 2)));
              Term::Lam {
                tag: self.labels.con.to_tag(*lab),
                pat: Box::new(Pattern::Var(nam)),
                bod: Box::new(bod),
              }
            }
          }
          // If we're visiting a port 1, then it is a variable.
          1 => Term::Var { nam: self.namegen.var_name(next) },
          // If we're visiting a port 2, then it is an application.
          2 => {
            let fun = self.read_term(self.net.enter_port(Port(node, 0)));
            let arg = self.read_term(self.net.enter_port(Port(node, 1)));
            Term::App { tag: self.labels.con.to_tag(*lab), fun: Box::new(fun), arg: Box::new(arg) }
          }
          _ => unreachable!(),
        },
        NodeKind::Mat => match next.slot() {
          2 => {
            // Read the matched expression
            let arg = self.read_term(self.net.enter_port(Port(node, 0)));
            let bnd = if let Term::Var { nam } = &arg { nam.clone() } else { self.namegen.unique() };

            // Read the pattern matching node
            let sel_node = self.net.enter_port(Port(node, 1)).node();

            // We expect the pattern matching node to be a CON
            let sel_kind = &self.net.node(sel_node).kind;
            let (zero, succ) = if *sel_kind == NodeKind::Ctr(Con(None)) {
              let zero_term = self.read_term(self.net.enter_port(Port(sel_node, 1)));
              let mut succ_term = self.read_term(self.net.enter_port(Port(sel_node, 2)));

              match &mut succ_term {
                Term::Lam { pat: box Pattern::Var(nam), bod, .. } => {
                  let mut bod = std::mem::take(bod.as_mut());
                  if let Some(nam) = &nam {
                    bod.subst(nam, &Term::Var { nam: Name::new(format!("{bnd}-1")) });
                  }
                  (zero_term, bod)
                }
                _ => {
                  self.error(ReadbackError::InvalidNumericMatch);
                  (zero_term, succ_term)
                }
              }
            } else {
              // TODO: Is there any case where we expect a different node type here on readback?
              self.error(ReadbackError::InvalidNumericMatch);
              (Term::Err, Term::Err)
            };
            Term::Swt { arg: Box::new(arg), bnd: Some(bnd), with: vec![], pred: None, arms: vec![zero, succ] }
          }
          _ => {
            self.error(ReadbackError::InvalidNumericMatch);
            Term::Err
          }
        },
        NodeKind::Ref { def_name } => Term::Ref { nam: def_name.clone() },
        // If we're visiting a fan node...
        NodeKind::Ctr(kind @ (Dup(_) | Tup(_))) => {
          let (fan, lab) = match *kind {
            Tup(lab) => (FanKind::Tup, lab),
            Dup(lab) => (FanKind::Dup, Some(lab)),
            _ => unreachable!(),
          };
          match next.slot() {
            // If we're visiting a port 0, then it is a pair.
            0 => {
              if fan == FanKind::Dup
                && let Some(dup_paths) = &mut self.dup_paths
                && let stack = dup_paths.entry(lab.unwrap()).or_default()
                && let Some(slot) = stack.pop()
              {
                // Since we had a paired Dup in the path to this Sup,
                // we "decay" the superposition according to the original direction we came from the Dup.
                let term = self.read_term(self.net.enter_port(Port(node, slot)));
                self.dup_paths.as_mut().unwrap().get_mut(&lab.unwrap()).unwrap().push(slot);
                term
              } else {
                // If no Dup with same label in the path, we can't resolve the Sup, so keep it as a term.
                self.decay_or_get_ports(node).map_or_else(
                  |(fst, snd)| Term::Fan { fan, tag: self.labels[fan].to_tag(lab), els: vec![fst, snd] },
                  |term| term,
                )
              }
            }
            // If we're visiting a port 1 or 2, then it is a variable.
            // Also, that means we found a dup, so we store it to read later.
            1 | 2 => {
              if fan == FanKind::Dup
                && let Some(dup_paths) = &mut self.dup_paths
              {
                dup_paths.entry(lab.unwrap()).or_default().push(next.slot());
                let term = self.read_term(self.net.enter_port(Port(node, 0)));
                self.dup_paths.as_mut().unwrap().entry(lab.unwrap()).or_default().pop().unwrap();
                term
              } else {
                if self.seen_fans.insert(node) {
                  self.scope.insert(node);
                }
                Term::Var { nam: self.namegen.var_name(next) }
              }
            }
            _ => unreachable!(),
          }
        }
        NodeKind::Num { val: _ } => {
          let (flp, arg) = self.read_opr_arg(next);
          match arg {
            NumArg::Sym(opr) => Term::Oper {
              opr: Op::from_native_tag(opr, NumType::U24),
              fst: Box::new(Term::Err),
              snd: Box::new(Term::Err),
            },
            NumArg::Num(typ, val) => Term::Num { val: Num::from_bits_and_type(val, typ) },
            NumArg::Par(opr, val) => {
              if flp {
                Term::Oper {
                  opr: Op::from_native_tag(opr, NumType::U24),
                  fst: Box::new(Term::Num { val: Num::from_bits_and_type(val, NumType::U24) }),
                  snd: Box::new(Term::Err),
                }
              } else {
                Term::Oper {
                  opr: Op::from_native_tag(opr, NumType::U24),
                  fst: Box::new(Term::Err),
                  snd: Box::new(Term::Num { val: Num::from_bits_and_type(val, NumType::U24) }),
                }
              }
            }
            NumArg::Oth(_) => unreachable!(),
          }
        }
        NodeKind::Opr => match next.slot() {
          2 => {
            let port0_kind = self.net.node(self.net.enter_port(Port(node, 0)).node()).kind.clone();
            if port0_kind == NodeKind::Opr {
              // Second half of a numeric operation
              let fst = self.read_term(self.net.enter_port(Port(node, 0)));
              if let Term::Oper { opr, fst, snd: _ } = &fst {
                let (flip, arg) = self.read_opr_arg(self.net.enter_port(Port(node, 1)));
                let snd = Box::new(match arg {
                  NumArg::Num(typ, val) => Term::Num { val: Num::from_bits_and_type(val, typ) },
                  NumArg::Oth(term) => term,
                  NumArg::Sym(_) | NumArg::Par(_, _) => {
                    self.error(ReadbackError::InvalidNumericOp);
                    Term::Err
                  }
                });
                let (fst, snd) = if flip { (snd, fst.clone()) } else { (fst.clone(), snd) };
                Term::Oper { opr: *opr, fst, snd }
              } else {
                self.error(ReadbackError::InvalidNumericOp);
                Term::Err
              }
            } else {
              // First half of a numeric operation
              let (flip0, arg0) = self.read_opr_arg(self.net.enter_port(Port(node, 0)));
              let (flip1, arg1) = self.read_opr_arg(self.net.enter_port(Port(node, 1)));
              let (arg0, arg1) = if flip0 != flip1 { (arg1, arg0) } else { (arg0, arg1) };
              match (arg0, arg1) {
                (NumArg::Sym(opr), NumArg::Num(typ, val)) | (NumArg::Num(typ, val), NumArg::Sym(opr)) => {
                  Term::Oper {
                    opr: Op::from_native_tag(opr, typ),
                    fst: Box::new(Term::Num { val: Num::from_bits_and_type(val, typ) }),
                    snd: Box::new(Term::Err),
                  }
                }
                (NumArg::Num(typ, num1), NumArg::Par(opr, num2))
                | (NumArg::Par(opr, num1), NumArg::Num(typ, num2)) => Term::Oper {
                  opr: Op::from_native_tag(opr, typ),
                  fst: Box::new(Term::Num { val: Num::from_bits_and_type(num1, typ) }),
                  snd: Box::new(Term::Num { val: Num::from_bits_and_type(num2, typ) }),
                },
                // No type, so assuming u24
                (NumArg::Sym(opr), NumArg::Oth(term)) | (NumArg::Oth(term), NumArg::Sym(opr)) => Term::Oper {
                  opr: Op::from_native_tag(opr, NumType::U24),
                  fst: Box::new(term),
                  snd: Box::new(Term::Err),
                },

                (NumArg::Par(opr, num), NumArg::Oth(term)) => Term::Oper {
                  opr: Op::from_native_tag(opr, NumType::U24),
                  fst: Box::new(Term::Num { val: Num::from_bits_and_type(num, NumType::U24) }),
                  snd: Box::new(term),
                },
                (NumArg::Oth(term), NumArg::Par(opr, num)) => Term::Oper {
                  opr: Op::from_native_tag(opr, NumType::U24),
                  fst: Box::new(term),
                  snd: Box::new(Term::Num { val: Num::from_bits_and_type(num, NumType::U24) }),
                },
                _ => {
                  self.error(ReadbackError::InvalidNumericOp);
                  Term::Err
                }
              }
            }
          }
          _ => {
            self.error(ReadbackError::InvalidNumericOp);
            Term::Err
          }
        },
        NodeKind::Rot => {
          self.error(ReadbackError::ReachedRoot);
          Term::Err
        }
      }
    })
  }

  fn read_opr_arg(&mut self, next: Port) -> (bool, NumArg) {
    let node = next.node();
    match &self.net.node(node).kind {
      NodeKind::Num { val } => {
        self.seen.insert(next);
        let flipped = ((val >> 28) & 0x1) != 0;
        let typ = val & 0xf;
        let val = (val >> 4) & 0x00ff_ffff;
        let arg = match typ {
          // Sym
          0x0 => NumArg::Sym(val & 0xf),
          // Num
          0x1 ..= 0x3 => NumArg::Num(NumType::from_native_tag(typ), val),
          // Partial
          opr => NumArg::Par(opr, val),
        };
        (flipped, arg)
      }
      _ => {
        let term = self.read_term(next);
        (false, NumArg::Oth(term))
      }
    }
  }

  /// Enters both ports 1 and 2 of a node. Returns a Term if it is
  /// possible to simplify the net, or the Terms on the two ports of the node.
  /// The two possible outcomes are always equivalent.
  ///
  /// If:
  ///  - The node Kind is CON/TUP/DUP
  ///  - Both ports 1 and 2 are connected to the same node on slots 1 and 2 respectively
  ///  - That node Kind is the same as the given node Kind
  ///
  /// Then:
  ///   Reads the port 0 of the connected node, and returns that term.
  ///
  /// Otherwise:
  ///   Returns the terms on ports 1 and 2 of the given node.
  ///
  /// # Example
  ///
  /// ```hvm
  /// // λa let (a, b) = a; (a, b)
  /// ([a b] [a b])
  ///
  /// // The node `(a, b)` is just a reconstruction of the destructuring of `a`,
  /// // So we can skip both steps and just return the "value" unchanged:
  ///
  /// // λa a
  /// (a a)
  /// ```
  ///
  fn decay_or_get_ports(&mut self, node: NodeId) -> Result<Term, (Term, Term)> {
    let fst_port = self.net.enter_port(Port(node, 1));
    let snd_port = self.net.enter_port(Port(node, 2));

    let node_kind = &self.net.node(node).kind;

    // Eta-reduce the readback inet.
    // This is not valid for all kinds of nodes, only CON/TUP/DUP, due to their interaction rules.
    if matches!(node_kind, NodeKind::Ctr(_)) {
      match (fst_port, snd_port) {
        (Port(fst_node, 1), Port(snd_node, 2)) if fst_node == snd_node => {
          if self.net.node(fst_node).kind == *node_kind {
            self.scope.remove(&fst_node);

            let port_zero = self.net.enter_port(Port(fst_node, 0));
            let term = self.read_term(port_zero);
            return Ok(term);
          }
        }
        _ => {}
      }
    }

    let fst = self.read_term(fst_port);
    let snd = self.read_term(snd_port);
    Err((fst, snd))
  }

  pub fn error(&mut self, error: ReadbackError) {
    self.errors.push(error);
  }

  pub fn report_errors(&mut self, diagnostics: &mut Diagnostics) {
    let mut err_counts = std::collections::HashMap::new();
    for err in &self.errors {
      *err_counts.entry(*err).or_insert(0) += 1;
    }

    for (err, count) in err_counts {
      let count_msg = if count > 1 { format!(" ({count} occurrences)") } else { "".to_string() };
      let msg = format!("{}{}", err, count_msg);
      diagnostics.add_diagnostic(msg.as_str(), Severity::Warning, DiagnosticOrigin::Readback);
    }
  }

  /// Returns whether the given port represents a tuple or some other
  /// term (usually a lambda).
  ///
  /// Used heuristic: a con node is a tuple if port 1 is a closed net and not an ERA.
  fn is_tup(&self, node: NodeId) -> bool {
    if !matches!(self.net.node(node).kind, NodeKind::Ctr(CtrKind::Con(_))) {
      return false;
    }
    if self.net.node(self.net.enter_port(Port(node, 1)).node()).kind == NodeKind::Era {
      return false;
    }
    let mut wires = HashSet::new();
    let mut to_check = vec![self.net.enter_port(Port(node, 1))];
    while let Some(port) = to_check.pop() {
      match port.slot() {
        0 => {
          let node = port.node();
          let lft = self.net.enter_port(Port(node, 1));
          let rgt = self.net.enter_port(Port(node, 2));
          to_check.push(lft);
          to_check.push(rgt);
        }
        1 | 2 => {
          // Mark as a wire. If already present, mark as visited by removing it.
          if !(wires.insert(port) && wires.insert(self.net.enter_port(port))) {
            wires.remove(&port);
            wires.remove(&self.net.enter_port(port));
          }
        }
        _ => unreachable!(),
      }
    }
    // No hanging wires = a combinator = a tuple
    wires.is_empty()
  }
}

/// Argument for a Opr node
enum NumArg {
  Sym(u32),
  Num(NumType, u32),
  Par(u32, u32),
  Oth(Term),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumType {
  U24 = 1,
  I24 = 2,
  F24 = 3,
}

impl Op {
  fn from_native_tag(val: u32, typ: NumType) -> Op {
    match val {
      0x4 => Op::ADD,
      0x5 => Op::SUB,
      0x6 => Op::MUL,
      0x7 => Op::DIV,
      0x8 => Op::REM,
      0x9 => Op::EQL,
      0xa => Op::NEQ,
      0xb => Op::LTN,
      0xc => Op::GTN,
      0xd => {
        if typ == NumType::F24 {
          Op::ATN
        } else {
          Op::AND
        }
      }
      0xe => {
        if typ == NumType::F24 {
          Op::LOG
        } else {
          Op::OR
        }
      }
      0xf => {
        if typ == NumType::F24 {
          Op::POW
        } else {
          Op::XOR
        }
      }
      _ => unreachable!(),
    }
  }
}

impl NumType {
  fn from_native_tag(value: u32) -> Self {
    match value {
      x if x == NumType::U24 as u32 => NumType::U24,
      x if x == NumType::I24 as u32 => NumType::I24,
      x if x == NumType::F24 as u32 => NumType::F24,
      _ => unreachable!(),
    }
  }
}

impl Num {
  fn from_bits_and_type(bits: u32, typ: NumType) -> Self {
    Num::from_bits((bits & 0x00ff_ffff) << 4 | (typ as u32))
  }
}

/* Insertion of dups in the middle of the term */

/// Represents `let #tag(fst, snd) = val` / `let #tag{fst snd} = val`
struct Split {
  fan: FanKind,
  tag: Tag,
  fst: Option<Name>,
  snd: Option<Name>,
  val: Term,
}

impl Default for Split {
  fn default() -> Self {
    Self {
      fan: FanKind::Dup,
      tag: Default::default(),
      fst: Default::default(),
      snd: Default::default(),
      val: Default::default(),
    }
  }
}

impl Term {
  /// Calculates the number of times `fst` and `snd` appear in this term. If
  /// that is `>= threshold`, it inserts the split at this term, and returns
  /// `None`. Otherwise, returns `Some(uses)`.
  ///
  /// This is only really useful when called in two passes – first, with
  /// `threshold = usize::MAX`, to count the number of uses, and then with
  /// `threshold = uses`.
  ///
  /// This has the effect of inserting the split at the lowest common ancestor
  /// of all of the uses of `fst` and `snd`.
  fn insert_split(&mut self, split: &mut Split, threshold: usize) -> Option<usize> {
    maybe_grow(|| {
      let mut n = match self {
        Term::Var { nam } => usize::from(split.fst == *nam || split.snd == *nam),
        _ => 0,
      };
      for child in self.children_mut() {
        n += child.insert_split(split, threshold)?;
      }

      if n >= threshold {
        let Split { fan, tag, fst, snd, val } = std::mem::take(split);
        let nxt = Box::new(std::mem::take(self));
        *self = Term::Let {
          pat: Box::new(Pattern::Fan(fan, tag, vec![Pattern::Var(fst), Pattern::Var(snd)])),
          val: Box::new(val),
          nxt,
        };
        None
      } else {
        Some(n)
      }
    })
  }
}

/* Variable name generation */

#[derive(Default)]
pub struct NameGen {
  pub var_port_to_id: HashMap<Port, u64>,
  pub id_counter: u64,
}

impl NameGen {
  // Given a port, returns its name, or assigns one if it wasn't named yet.
  fn var_name(&mut self, var_port: Port) -> Name {
    let id = self.var_port_to_id.entry(var_port).or_insert_with(|| {
      let id = self.id_counter;
      self.id_counter += 1;
      id
    });
    Name::from(*id)
  }

  fn decl_name(&mut self, net: &INet, var_port: Port) -> Option<Name> {
    // If port is linked to an erase node, return an unused variable
    let var_use = net.enter_port(var_port);
    let var_kind = &net.node(var_use.node()).kind;
    (*var_kind != NodeKind::Era).then(|| self.var_name(var_port))
  }

  pub fn unique(&mut self) -> Name {
    let id = self.id_counter;
    self.id_counter += 1;
    Name::from(id)
  }
}

/* Readback errors */

#[derive(Debug, Clone, Copy)]
pub enum ReadbackError {
  InvalidNumericMatch,
  InvalidNumericOp,
  ReachedRoot,
  Cyclic,
}

impl PartialEq for ReadbackError {
  fn eq(&self, other: &Self) -> bool {
    core::mem::discriminant(self) == core::mem::discriminant(other)
  }
}

impl Eq for ReadbackError {}

impl std::hash::Hash for ReadbackError {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    core::mem::discriminant(self).hash(state);
  }
}

impl std::fmt::Display for ReadbackError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ReadbackError::InvalidNumericMatch => write!(f, "Encountered an invalid 'switch'."),
      ReadbackError::InvalidNumericOp => write!(f, "Encountered an invalid numeric operation."),
      ReadbackError::ReachedRoot => {
        write!(f, "Unable to interpret the HVM result as a valid Bend term. (Reached Root)")
      }
      ReadbackError::Cyclic => {
        write!(f, "Unable to interpret the HVM result as a valid Bend term. (Cyclic Term)")
      }
    }
  }
}

/* Recover unscoped vars */

impl Term {
  pub fn collect_unscoped(&self, unscoped: &mut HashSet<Name>, scope: &mut Vec<Name>) {
    maybe_grow(|| match self {
      Term::Var { nam } if !scope.contains(nam) => _ = unscoped.insert(nam.clone()),
      Term::Swt { arg, bnd, with: _, pred: _, arms } => {
        arg.collect_unscoped(unscoped, scope);
        arms[0].collect_unscoped(unscoped, scope);
        if let Some(bnd) = bnd {
          scope.push(Name::new(format!("{bnd}-1")));
        }
        arms[1].collect_unscoped(unscoped, scope);
        if bnd.is_some() {
          scope.pop();
        }
      }
      _ => {
        for (child, binds) in self.children_with_binds() {
          let binds: Vec<_> = binds.collect();
          for bind in binds.iter().copied().flatten() {
            scope.push(bind.clone());
          }
          child.collect_unscoped(unscoped, scope);
          for _bind in binds.into_iter().flatten() {
            scope.pop();
          }
        }
      }
    })
  }

  pub fn apply_unscoped(&mut self, unscoped: &HashSet<Name>) {
    maybe_grow(|| {
      if let Term::Var { nam } = self
        && unscoped.contains(nam)
      {
        *self = Term::Link { nam: std::mem::take(nam) }
      }
      if let Some(pat) = self.pattern_mut() {
        pat.apply_unscoped(unscoped);
      }
      for child in self.children_mut() {
        child.apply_unscoped(unscoped);
      }
    })
  }
}

impl Pattern {
  fn apply_unscoped(&mut self, unscoped: &HashSet<Name>) {
    maybe_grow(|| {
      if let Pattern::Var(Some(nam)) = self
        && unscoped.contains(nam)
      {
        let nam = std::mem::take(nam);
        *self = Pattern::Chn(nam);
      }
      for child in self.children_mut() {
        child.apply_unscoped(unscoped)
      }
    })
  }
}
