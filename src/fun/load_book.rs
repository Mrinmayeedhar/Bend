use super::{parser::TermParser, Book, Name};
use std::{fmt::Display, path::Path};

// TODO: Refactor so that we don't mix the two syntaxes here.

/// Reads a file and parses to a definition book.
pub fn load_file_to_book(
  path: &Path,
  package_loader: impl Fn(&Name, &[Name]) -> Result<String, String>,
) -> Result<Book, String> {
  let code = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
  load_to_book(path.display(), &code, package_loader)
}

pub fn load_to_book<T: Display>(
  origin: T,
  code: &str,
  package_loader: impl Fn(&Name, &[Name]) -> Result<String, String>,
) -> Result<Book, String> {
  let builtins = Book::default();
  let mut book = do_parse_book(code, origin, builtins)?;

  load_imports_to_book(&mut book, &package_loader)?;
  Ok(book)
}

fn load_imports_to_book(
  book: &mut Book,
  package_loader: &impl Fn(&Name, &[Name]) -> Result<String, String>,
) -> Result<(), String> {
  for (src, sub_imports) in &book.imports.names {
    let code = package_loader(src, sub_imports)?;

    let mut module = TermParser::new(&code).parse_book(Book::default(), false)?;
    load_imports_to_book(&mut module, package_loader)?;

    let (_, name) = src.split_once('/').unwrap();
    book.imports.map.insert(Name::new(name), src.clone());
    book
      .imports
      .map
      .extend(sub_imports.iter().map(|sub| (sub.clone(), Name::new(format!("{}/{}", src, sub)))));
    book.imports.pkgs.push((src.clone(), module));
  }

  Ok(())
}

pub fn do_parse_book<T: Display>(code: &str, origin: T, builtins: Book) -> Result<Book, String> {
  TermParser::new(code).parse_book(builtins, false).map_err(|e| format!("In {} :\n{}", origin, e))
}
