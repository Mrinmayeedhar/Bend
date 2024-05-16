use crate::fun::{parser::TermParser, Book};
use std::path::Path;

// TODO: Refactor so that we don't mix the two syntaxes here.

/// Reads a file and parses to a definition book.
pub fn load_file_to_book(
  path: &Path,
  package_loader: impl Fn(&str) -> Result<String, String>,
) -> Result<Book, String> {
  let builtins = Book::builtins();
  let code = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
  let mut book = do_parse_book(&code, path, builtins)?;

  load_imports(&mut book, &package_loader)?;
  Ok(book)
}

fn load_imports(
  book: &mut Book,
  package_loader: &impl Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
  for import in &book.imports {
    let code = package_loader(import)?;
    let mut module = TermParser::new(&code).parse_book(Book::default(), false)?;
    load_imports(&mut module, package_loader)?;

    let (_, name) = import.split_once('/').unwrap();
    let name = super::Name::new(name);
    book.mods.insert(name, (import.clone(), module));
  }
  Ok(())
}

pub fn do_parse_book(code: &str, path: &Path, builtins: Book) -> Result<Book, String> {
  TermParser::new(code).parse_book(builtins, false).map_err(|e| format!("In {} :\n{}", path.display(), e))
}
