#![feature(let_chains)]

use lliw::Fg;
use std::fs;
use std::io::{Error, Write};
use std::process::Command;
use strum::EnumIs;
use synoptic;
use thiserror::Error;

pub mod ast;
pub mod codegen;
pub mod ir;
pub mod lexer;
#[cfg(feature = "llvm")]
pub mod llvm_ir;
pub mod semantic;
pub mod typecheck;

use cfg_if::cfg_if;

use lexer::TokenKind;

#[derive(Error, Debug)]
pub enum ErrorKind {
    #[error("Lexer Failed")]
    LexerError,
    #[error("AST Parsing Failed")]
    ParserError,
    #[error("Semantic Analysis Failed")]
    SemanticError,
    #[error("Type Checking Failed")]
    TypeCheckError,
    #[error("Codegen Failed")]
    CodegenError,
    #[error("Asm Emission Failed")]
    AsmEmitError,
    #[error("IO Error")]
    IOError,
}

type CompileResult<T> = Result<T, ErrorKind>;

#[derive(PartialEq, EnumIs, Clone, Copy)]
pub enum CompileStage {
    Lex,
    Parse,
    Validate,
    IR,
    Codegen,
    Full,
}

pub struct Driver {
    path: String,
    name: String,
}

impl Driver {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            name: path[..path.len() - 2].to_owned(),
        }
    }

    pub fn compile(&self, stage: CompileStage, link: bool, _llvm: bool) -> Result<(), ErrorKind> {
        let preprocessed_path = self.preprocess();
        let source = fs::read_to_string(preprocessed_path).unwrap();
        self.clean_preprocessed().unwrap();

        log::debug!("Preprocessed source:");
        log::debug!("\n{}", source);

        let tokens = self.lex(source).map_err(|_| ErrorKind::LexerError)?;
        log::debug!("Tokens:\n{:?}\n", &tokens);

        if stage.is_lex() {
            return Ok(());
        }

        let ast = self.parse(tokens).map_err(|_| ErrorKind::ParserError)?;
        log::debug!("Parsed AST:\n{}", ast);

        if stage.is_parse() {
            return Ok(());
        }

        let ast = ast.validate().map_err(|_| ErrorKind::SemanticError)?;
        log::trace!("Resolved and labelled AST:\n{}", ast);
        let ast = ast.typecheck().map_err(|_| ErrorKind::TypeCheckError)?;
        log::debug!("Validated AST:\n{}", ast);

        if stage.is_validate() {
            return Ok(());
        }

        let asm_path: Option<String>;
        cfg_if! {
            if #[cfg(feature = "llvm")] {
                if _llvm {
                    asm_path = self.llvm_asm_path(ast, stage);
                } else {
                    asm_path = self.asm_path(ast, stage)?;
                }
            } else {
                asm_path = self.asm_path(ast, stage)?;
            }
        }

        if let Some(p) = asm_path {
            self.assemble(&p, link);
            self.clean_asm().unwrap();
        }

        Ok(())
    }

    pub fn asm_path(
        &self,
        ast: ast::Program,
        stage: CompileStage,
    ) -> CompileResult<Option<String>> {
        let ir = self.generate_ir(ast);
        log::debug!("Generated IR:\n{}\n", &ir);

        if stage.is_ir() {
            return Ok(None);
        }

        let code = self.codegen(ir)?;
        log::trace!("Codegen:\n{}\n", &code);

        if stage.is_codegen() {
            return Ok(None);
        }

        Ok(Some(self.emit(code).unwrap()))
    }

    pub fn preprocess(&self) -> String {
        let output_path = self.path.replace(".c", ".i");

        let _ = Command::new("gcc")
            .args(["-E", "-P", &self.path, "-o", output_path.as_str()])
            .status()
            .expect("Failed to run the preprocessor");

        output_path
    }

    pub fn lex(&self, source: String) -> lexer::LexerResult<Vec<TokenKind>> {
        lexer::run_lexer(source)
    }

    pub fn parse(&self, tokens: Vec<TokenKind>) -> ast::ParserResult<ast::Program> {
        ast::Program::parse(tokens)
    }

    fn generate_ir(&self, ast: ast::Program) -> ir::Program {
        let mut ir_ctx = ir::IrCtx::new();
        ir::Program::generate(ast, &mut ir_ctx)
    }

    fn codegen(&self, ir: ir::Program) -> CompileResult<codegen::Program> {
        codegen::Program::codegen(ir).map_err(|_| ErrorKind::CodegenError)
    }

    fn emit(&self, code: codegen::Program) -> Result<String, ErrorKind> {
        let output_path = format!("{}.s", self.name);
        let asm = code.emit().map_err(|_| ErrorKind::AsmEmitError)?;

        if log::log_enabled!(log::Level::Debug) {
            log::debug!("Emitted asm:");
            Driver::print_asm_with_highlight(&asm);
        }

        let mut file = fs::File::create(&output_path).map_err(|_| ErrorKind::IOError)?;
        file.write_all(asm.as_bytes())
            .map_err(|_| ErrorKind::IOError)?;

        Ok(output_path)
    }

    fn print_asm_with_highlight(asm: &str) {
        fn colour(name: &str) -> Fg {
            match name {
                "comment" => Fg::LightBlack,
                "digit" => Fg::Purple,
                "string" => Fg::Green,
                "keyword" => Fg::Yellow,
                "function" => Fg::Red,
                _ => panic!("unknown token name"),
            }
        }

        let mut highlight = synoptic::from_extension("asm", 4).unwrap();
        let highlighted_asm = asm.split('\n').map(|line| line.to_string()).collect();
        highlight.run(&highlighted_asm);

        for (line_number, line) in highlighted_asm.iter().enumerate() {
            print!("{}\t", line_number + 1);
            // Line returns tokens for the corresponding line
            for token in highlight.line(line_number, &line) {
                // Tokens can either require highlighting or not require highlighting
                match token {
                    synoptic::TokOpt::Some(text, kind) => {
                        print!("{}{text}{}", colour(&kind), Fg::Reset)
                    }
                    synoptic::TokOpt::None(text) => print!("{text}"),
                }
            }
            println!();
        }
    }

    fn assemble(&self, path: &str, link: bool) {
        let out_name = if link {
            self.name.clone()
        } else {
            format!("{}.o", self.name)
        };

        let args = if link {
            vec![path, "-masm=intel", "-g", "-o", &out_name]
        } else {
            vec!["-c", path, "-masm=intel", "-g", "-o", &out_name]
        };

        Command::new("gcc")
            .args(args)
            .status()
            .expect("Failed to run the assembler");
    }

    fn clean_asm(&self) -> Result<(), Error> {
        fs::remove_file(format!("{}.s", self.name))?;
        Ok(())
    }

    pub fn clean_preprocessed(&self) -> Result<(), Error> {
        fs::remove_file(format!("{}.i", self.name))?;
        Ok(())
    }

    pub fn clean_binary(&self) -> Result<(), Error> {
        fs::remove_file(format!("{}", self.name))?;
        Ok(())
    }

    // *** LLVM-specific methods *** //

    #[cfg(feature = "llvm")]
    fn generate_llvm_ir(&self, ast: ast::Program) -> String {
        ast.to_llvm(&self.name)
    }

    #[cfg(feature = "llvm")]
    pub fn llvm_asm_path(&self, ast: ast::Program, stage: CompileStage) -> Option<String> {
        let ir = self.generate_llvm_ir(ast);
        log::debug!("Generated LLVM IR:\n{}\n", &ir);

        if stage.is_ir() {
            return None;
        }

        let llvm_ir_path = self.emit_llvm(&ir).unwrap();

        let path = self.llvm_asm(&llvm_ir_path);
        let asm = fs::read_to_string(&path).unwrap();
        log::debug!("Emitted assembly:\n\n{}", asm);

        self.clean_llvm().unwrap();
        Some(path)
    }

    #[cfg(feature = "llvm")]
    fn llvm_asm(&self, path: &str) -> String {
        let output_path = format!("{}.s", self.name);
        let _ = Command::new("llc")
            .args([path, "-o", &output_path])
            .status()
            .expect("Failed to run llc");
        output_path
    }

    #[cfg(feature = "llvm")]
    fn emit_llvm(&self, llvm_ir: &str) -> Result<String, Error> {
        let output_path = format!("{}.ll", self.name);
        let mut file = fs::File::create(&output_path)?;
        file.write_all(llvm_ir.as_bytes())?;
        Ok(output_path)
    }

    #[cfg(feature = "llvm")]
    fn clean_llvm(&self) -> Result<(), Error> {
        fs::remove_file(format!("{}.ll", self.name))?;
        Ok(())
    }
}
