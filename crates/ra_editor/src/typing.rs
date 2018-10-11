use std::mem;

use ra_syntax::{
    TextUnit, TextRange, SyntaxNodeRef, File, AstNode, SyntaxKind,
    ast,
    algo::{
        find_covering_node, find_leaf_at_offset, LeafAtOffset,
    },
    text_utils::{intersect, contains_offset_nonstrict},
    SyntaxKind::*,
};

use {LocalEdit, EditBuilder, find_node_at_offset};

pub fn join_lines(file: &File, range: TextRange) -> LocalEdit {
    let range = if range.is_empty() {
        let syntax = file.syntax();
        let text = syntax.text().slice(range.start()..);
        let pos = match text.find('\n') {
            None => return LocalEdit {
                edit: EditBuilder::new().finish(),
                cursor_position: None
            },
            Some(pos) => pos
        };
        TextRange::offset_len(
            range.start() + pos,
            TextUnit::of_char('\n'),
        )
    } else {
        range
    };

    let node = find_covering_node(file.syntax(), range);
    let mut edit = EditBuilder::new();
    for node in node.descendants() {
        let text = match node.leaf_text() {
            Some(text) => text,
            None => continue,
        };
        let range = match intersect(range, node.range()) {
            Some(range) => range,
            None => continue,
        } - node.range().start();
        for (pos, _) in text[range].bytes().enumerate().filter(|&(_, b)| b == b'\n') {
            let pos: TextUnit = (pos as u32).into();
            let off = node.range().start() + range.start() + pos;
            if !edit.invalidates_offset(off) {
                remove_newline(&mut edit, node, text.as_str(), off);
            }
        }
    }

    LocalEdit {
        edit: edit.finish(),
        cursor_position: None,
    }
}

pub fn on_enter(file: &File, offset: TextUnit) -> Option<LocalEdit> {
    let comment = find_leaf_at_offset(file.syntax(), offset).left_biased().and_then(|it| ast::Comment::cast(it))?;

    if let ast::CommentFlavor::Multiline = comment.flavor() {
        return None;
    }

    let prefix = comment.prefix();
    if offset < comment.syntax().range().start() + TextUnit::of_str(prefix) + TextUnit::from(1) {
        return None;
    }

    let indent = node_indent(file, comment.syntax())?;
    let inserted = format!("\n{}{} ", indent, prefix);
    let cursor_position = offset + TextUnit::of_str(&inserted);
    let mut edit = EditBuilder::new();
    edit.insert(offset, inserted);
    Some(LocalEdit {
        edit: edit.finish(),
        cursor_position: Some(cursor_position),
    })
}

fn node_indent<'a>(file: &'a File, node: SyntaxNodeRef) -> Option<&'a str> {
    let ws = match find_leaf_at_offset(file.syntax(), node.range().start()) {
        LeafAtOffset::Between(l, r) => {
            assert!(r == node);
            l
        }
        LeafAtOffset::Single(n) => {
            assert!(n == node);
            return Some("")
        }
        LeafAtOffset::None => unreachable!(),
    };
    if ws.kind() != WHITESPACE {
        return None;
    }
    let text = ws.leaf_text().unwrap();
    let pos = text.as_str().rfind('\n').map(|it| it + 1).unwrap_or(0);
    Some(&text[pos..])
}

pub fn on_eq_typed(file: &File, offset: TextUnit) -> Option<LocalEdit> {
    let let_stmt: ast::LetStmt = find_node_at_offset(file.syntax(), offset)?;
    if let_stmt.has_semi() {
        return None;
    }
    if let Some(expr) = let_stmt.initializer() {
        let expr_range = expr.syntax().range();
        if contains_offset_nonstrict(expr_range, offset) && offset != expr_range.start() {
            return None;
        }
        if file.syntax().text().slice(offset..expr_range.start()).contains('\n') {
            return None;
        }
    } else {
        return None;
    }
    let offset = let_stmt.syntax().range().end();
    let mut edit = EditBuilder::new();
    edit.insert(offset, ";".to_string());
    Some(LocalEdit {
        edit: edit.finish(),
        cursor_position: None,
    })
}

fn remove_newline(
    edit: &mut EditBuilder,
    node: SyntaxNodeRef,
    node_text: &str,
    offset: TextUnit,
) {
    if node.kind() != WHITESPACE || node_text.bytes().filter(|&b| b == b'\n').count() != 1 {
        // The node is either the first or the last in the file
        let suff = &node_text[TextRange::from_to(
            offset - node.range().start() + TextUnit::of_char('\n'),
            TextUnit::of_str(node_text),
        )];
        let spaces = suff.bytes().take_while(|&b| b == b' ').count();

        edit.replace(
            TextRange::offset_len(offset, ((spaces + 1) as u32).into()),
            " ".to_string(),
        );
        return;
    }

    // Special case that turns something like:
    //
    // ```
    // my_function({<|>
    //    <some-expr>
    // })
    // ```
    //
    // into `my_function(<some-expr>)`
    if join_single_expr_block(edit, node).is_some() {
        return
    }

    // The node is between two other nodes
    let prev = node.prev_sibling().unwrap();
    let next = node.next_sibling().unwrap();
    if is_trailing_comma(prev.kind(), next.kind()) {
        // Removes: trailing comma, newline (incl. surrounding whitespace)
        edit.delete(TextRange::from_to(prev.range().start(), node.range().end()));
    } else if prev.kind() == COMMA && next.kind() == R_CURLY {
        // Removes: comma, newline (incl. surrounding whitespace)
        // Adds: a single whitespace
        edit.replace(
            TextRange::from_to(prev.range().start(), node.range().end()),
            " ".to_string()
        );
    } else if let (Some(_), Some(next)) = (ast::Comment::cast(prev), ast::Comment::cast(next)) {
        // Removes: newline (incl. surrounding whitespace), start of the next comment
        let comment_text = next.text();
        if let Some(newline_pos) = comment_text.find('\n') {
            // Special case for multiline comments: join the comment content but
            // keep the leading `/*`

            let newline_offset = next.syntax().range().start()
                                + TextUnit::from(newline_pos as u32)
                                + TextUnit::of_char('\n');

            edit.insert(newline_offset, "/*".to_string());
            edit.delete(TextRange::from_to(
                node.range().start(),
                next.syntax().range().start() + TextUnit::of_str(next.prefix())
            ));
        } else {
            // Single-line comments
            edit.delete(TextRange::from_to(
                node.range().start(),
                next.syntax().range().start() + TextUnit::of_str(next.prefix())
            ));
        }
    } else {
        // Remove newline but add a computed amount of whitespace characters
        edit.replace(
            node.range(),
            compute_ws(prev, next).to_string(),
        );
    }
}

fn is_trailing_comma(left: SyntaxKind, right: SyntaxKind) -> bool {
    match (left, right) {
       (COMMA, R_PAREN) | (COMMA, R_BRACK) => true,
       _ => false
    }
}

fn join_single_expr_block(
    edit: &mut EditBuilder,
    node: SyntaxNodeRef,
) -> Option<()> {
    let block = ast::Block::cast(node.parent()?)?;
    let block_expr = ast::BlockExpr::cast(block.syntax().parent()?)?;
    let expr = single_expr(block)?;
    edit.replace(
        block_expr.syntax().range(),
        expr.syntax().text().to_string(),
    );
    Some(())
}

fn single_expr(block: ast::Block) -> Option<ast::Expr> {
    let mut res = None;
    for child in block.syntax().children() {
        if let Some(expr) = ast::Expr::cast(child) {
            if expr.syntax().text().contains('\n') {
                return None;
            }
            if mem::replace(&mut res, Some(expr)).is_some() {
                return None;
            }
        } else {
            match child.kind() {
                WHITESPACE | L_CURLY | R_CURLY => (),
                _ => return None,
            }
        }
    }
    res
}

fn compute_ws(left: SyntaxNodeRef, right: SyntaxNodeRef) -> &'static str {
    match left.kind() {
        L_PAREN | L_BRACK => return "",
        _ => (),
    }
    match right.kind() {
        R_PAREN | R_BRACK => return "",
        DOT => return "",
        _ => (),
    }
    " "
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_utils::{check_action, extract_range, extract_offset, add_cursor};

    fn check_join_lines(before: &str, after: &str) {
        check_action(before, after, |file, offset| {
            let range = TextRange::offset_len(offset, 0.into());
            let res = join_lines(file, range);
            Some(res)
        })
    }

    #[test]
    fn test_join_lines_comma() {
        check_join_lines(r"
fn foo() {
    <|>foo(1,
    )
}
", r"
fn foo() {
    <|>foo(1)
}
");
    }

    #[test]
    fn test_join_lines_lambda_block() {
        check_join_lines(r"
pub fn reparse(&self, edit: &AtomEdit) -> File {
    <|>self.incremental_reparse(edit).unwrap_or_else(|| {
        self.full_reparse(edit)
    })
}
", r"
pub fn reparse(&self, edit: &AtomEdit) -> File {
    <|>self.incremental_reparse(edit).unwrap_or_else(|| self.full_reparse(edit))
}
");
    }

    #[test]
    fn test_join_lines_block() {
        check_join_lines(r"
fn foo() {
    foo(<|>{
        92
    })
}", r"
fn foo() {
    foo(<|>92)
}");
    }

    fn check_join_lines_sel(before: &str, after: &str) {
        let (sel, before) = extract_range(before);
        let file = File::parse(&before);
        let result = join_lines(&file, sel);
        let actual = result.edit.apply(&before);
        assert_eq_text!(after, &actual);
    }

    #[test]
    fn test_join_lines_selection_fn_args() {
        check_join_lines_sel(r"
fn foo() {
    <|>foo(1,
        2,
        3,
    <|>)
}
    ", r"
fn foo() {
    foo(1, 2, 3)
}
    ");
    }

    #[test]
    fn test_join_lines_selection_struct() {
        check_join_lines_sel(r"
struct Foo <|>{
    f: u32,
}<|>
    ", r"
struct Foo { f: u32 }
    ");
    }

    #[test]
    fn test_join_lines_selection_dot_chain() {
        check_join_lines_sel(r"
fn foo() {
    join(<|>type_params.type_params()
            .filter_map(|it| it.name())
            .map(|it| it.text())<|>)
}", r"
fn foo() {
    join(type_params.type_params().filter_map(|it| it.name()).map(|it| it.text()))
}");
    }

    #[test]
    fn test_join_lines_selection_lambda_block_body() {
        check_join_lines_sel(r"
pub fn handle_find_matching_brace() {
    params.offsets
        .map(|offset| <|>{
            world.analysis().matching_brace(&file, offset).unwrap_or(offset)
        }<|>)
        .collect();
}", r"
pub fn handle_find_matching_brace() {
    params.offsets
        .map(|offset| world.analysis().matching_brace(&file, offset).unwrap_or(offset))
        .collect();
}");
    }

    #[test]
    fn test_on_eq_typed() {
        fn do_check(before: &str, after: &str) {
            let (offset, before) = extract_offset(before);
            let file = File::parse(&before);
            let result = on_eq_typed(&file, offset).unwrap();
            let actual = result.edit.apply(&before);
            assert_eq_text!(after, &actual);
        }

        //     do_check(r"
        // fn foo() {
        //     let foo =<|>
        // }
        // ", r"
        // fn foo() {
        //     let foo =;
        // }
        // ");
        do_check(r"
fn foo() {
    let foo =<|> 1 + 1
}
", r"
fn foo() {
    let foo = 1 + 1;
}
");
        //     do_check(r"
        // fn foo() {
        //     let foo =<|>
        //     let bar = 1;
        // }
        // ", r"
        // fn foo() {
        //     let foo =;
        //     let bar = 1;
        // }
        // ");
    }

    #[test]
    fn test_on_enter() {
        fn apply_on_enter(before: &str) -> Option<String> {
            let (offset, before) = extract_offset(before);
            let file = File::parse(&before);
            let result = on_enter(&file, offset)?;
            let actual = result.edit.apply(&before);
            let actual = add_cursor(&actual, result.cursor_position.unwrap());
            Some(actual)
        }

        fn do_check(before: &str, after: &str) {
            let actual = apply_on_enter(before).unwrap();
            assert_eq_text!(after, &actual);
        }

        fn do_check_noop(text: &str) {
            assert!(apply_on_enter(text).is_none())
        }

        do_check(r"
/// Some docs<|>
fn foo() {
}
", r"
/// Some docs
/// <|>
fn foo() {
}
");
        do_check(r"
impl S {
    /// Some<|> docs.
    fn foo() {}
}
", r"
impl S {
    /// Some
    /// <|> docs.
    fn foo() {}
}
");
        do_check_noop(r"<|>//! docz");
    }
}
