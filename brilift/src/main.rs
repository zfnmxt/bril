use argh::FromArgs;
use bril_rs as bril;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::InstBuilder;
use cranelift_codegen::settings::Configurable;
use cranelift_codegen::{ir, isa, settings};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use enum_map::{enum_map, Enum, EnumMap};
use std::collections::HashMap;
use std::fs;

#[derive(Debug, Enum)]
enum RTFunc {
    PrintInt,
    PrintBool,
    PrintSep,
    PrintEnd,
}

impl RTFunc {
    fn sig(&self) -> ir::Signature {
        match self {
            Self::PrintInt => ir::Signature {
                params: vec![ir::AbiParam::new(ir::types::I64)],
                returns: vec![],
                call_conv: isa::CallConv::SystemV,
            },
            Self::PrintBool => ir::Signature {
                params: vec![ir::AbiParam::new(ir::types::B1)],
                returns: vec![],
                call_conv: isa::CallConv::SystemV,
            },
            Self::PrintSep => ir::Signature {
                params: vec![],
                returns: vec![],
                call_conv: isa::CallConv::SystemV,
            },
            Self::PrintEnd => ir::Signature {
                params: vec![],
                returns: vec![],
                call_conv: isa::CallConv::SystemV,
            },
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::PrintInt => "print_int",
            Self::PrintBool => "print_bool",
            Self::PrintSep => "print_sep",
            Self::PrintEnd => "print_end",
        }
    }
}

#[derive(Debug, Enum)]
enum RTSetupFunc {
    ParseInt,
    ParseBool,
}

impl RTSetupFunc {
    fn sig(&self, pointer_type: ir::Type) -> ir::Signature {
        match self {
            Self::ParseInt => ir::Signature {
                params: vec![
                    ir::AbiParam::new(pointer_type),
                    ir::AbiParam::new(ir::types::I64),
                ],
                returns: vec![ir::AbiParam::new(ir::types::I64)],
                call_conv: isa::CallConv::SystemV,
            },
            Self::ParseBool => ir::Signature {
                params: vec![
                    ir::AbiParam::new(pointer_type),
                    ir::AbiParam::new(ir::types::I64),
                ],
                returns: vec![ir::AbiParam::new(ir::types::B1)],
                call_conv: isa::CallConv::SystemV,
            },
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::ParseInt => "parse_int",
            Self::ParseBool => "parse_bool",
        }
    }
}

fn translate_type(typ: &bril::Type) -> ir::Type {
    match typ {
        bril::Type::Int => ir::types::I64,
        bril::Type::Bool => ir::types::B1,
    }
}

fn translate_sig(func: &bril::Function) -> ir::Signature {
    let mut sig = ir::Signature::new(isa::CallConv::SystemV);
    if let Some(ret) = &func.return_type {
        sig.returns.push(ir::AbiParam::new(translate_type(ret)));
    }
    for arg in &func.args {
        sig.params
            .push(ir::AbiParam::new(translate_type(&arg.arg_type)));
    }
    sig
}

/// Get all the variables defined in a function (and their types), including the arguments.
fn all_vars(func: &bril::Function) -> HashMap<&String, &bril::Type> {
    func.instrs
        .iter()
        .filter_map(|inst| match inst {
            bril::Code::Instruction(op) => match op {
                bril::Instruction::Constant {
                    dest,
                    op: _,
                    const_type: typ,
                    value: _,
                } => Some((dest, typ)),
                bril::Instruction::Value {
                    args: _,
                    dest,
                    funcs: _,
                    labels: _,
                    op: _,
                    op_type: typ,
                } => Some((dest, typ)),
                _ => None,
            },
            _ => None,
        })
        .chain(func.args.iter().map(|arg| (&arg.name, &arg.arg_type)))
        .collect()
}

// TODO Should really be a trait with two different structs that implement it?
struct Translator<M: Module> {
    rt_funcs: EnumMap<RTFunc, cranelift_module::FuncId>,
    module: M,
    context: cranelift_codegen::Context,
    funcs: HashMap<String, cranelift_module::FuncId>,
    pointer_type: ir::Type,
}

fn declare_rt<M: Module>(module: &mut M) -> EnumMap<RTFunc, cranelift_module::FuncId> {
    enum_map! {
        rtfunc =>
            module
                .declare_function(
                    rtfunc.name(),
                    cranelift_module::Linkage::Import,
                    &rtfunc.sig(),
                )
                .unwrap()
    }
}

fn get_isa(
    target: Option<String>,
    pic: bool,
    opt_level: &str,
) -> Box<dyn cranelift_codegen::isa::TargetIsa> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", opt_level)
        .expect("invalid opt level");
    if pic {
        flag_builder.set("is_pic", "true").unwrap();
    }
    let isa_builder = if let Some(targ) = target {
        cranelift_codegen::isa::lookup_by_name(&targ).expect("invalid target")
    } else {
        cranelift_native::builder().unwrap()
    };
    isa_builder
        .finish(settings::Flags::new(flag_builder))
        .unwrap()
}

impl Translator<ObjectModule> {
    fn new(target: Option<String>, opt_level: &str) -> Self {
        // Make an object module.
        let isa = get_isa(target, true, opt_level);
        let pointer_type = isa.pointer_type();
        let mut module =
            ObjectModule::new(ObjectBuilder::new(isa, "foo", default_libcall_names()).unwrap());

        Self {
            rt_funcs: declare_rt(&mut module),
            module,
            context: cranelift_codegen::Context::new(),
            funcs: HashMap::new(),
            pointer_type,
        }
    }

    fn emit(self, output: &str) {
        let prod = self.module.finish();
        let objdata = prod.emit().expect("emission failed");
        fs::write(output, objdata).expect("failed to write .o file");
    }
}

impl Translator<JITModule> {
    fn new() -> Self {
        // Cranelift JIT scaffolding.
        let builder = JITBuilder::new(cranelift_module::default_libcall_names()).unwrap();
        let mut module = JITModule::new(builder);
        let pointer_type = module.isa().pointer_type();

        Self {
            rt_funcs: declare_rt(&mut module),
            context: module.make_context(),
            module,
            funcs: HashMap::new(),
            pointer_type,
        }
    }

    fn compile(mut self) -> *const u8 {
        self.module.clear_context(&mut self.context);
        self.module.finalize_definitions();

        // TODO Compile all functions.
        let id = self.funcs["main"];
        self.module.get_finalized_function(id)
    }
}

fn is_term(inst: &bril::Instruction) -> bool {
    if let bril::Instruction::Effect {
        args: _,
        funcs: _,
        labels: _,
        op,
    } = inst
    {
        matches!(
            op,
            bril::EffectOps::Branch | bril::EffectOps::Jump | bril::EffectOps::Return
        )
    } else {
        false
    }
}

fn gen_icmp(
    builder: &mut FunctionBuilder,
    vars: &HashMap<String, Variable>,
    args: &[String],
    dest: &String,
    cc: IntCC,
) {
    let lhs = builder.use_var(vars[&args[0]]);
    let rhs = builder.use_var(vars[&args[1]]);
    let res = builder.ins().icmp(cc, lhs, rhs);
    builder.def_var(vars[dest], res);
}

fn gen_binary(
    builder: &mut FunctionBuilder,
    vars: &HashMap<String, Variable>,
    args: &[String],
    dest: &String,
    dest_type: &bril::Type,
    op: ir::Opcode,
) {
    let lhs = builder.use_var(vars[&args[0]]);
    let rhs = builder.use_var(vars[&args[1]]);
    let typ = translate_type(dest_type);
    let (inst, dfg) = builder.ins().Binary(op, typ, lhs, rhs);
    let res = dfg.first_result(inst);
    builder.def_var(vars[dest], res);
}

struct CompileEnv {
    vars: HashMap<String, Variable>,
    var_types: HashMap<String, bril::Type>,
    rt_refs: EnumMap<RTFunc, ir::FuncRef>,
    blocks: HashMap<String, ir::Block>,
    func_refs: HashMap<String, ir::FuncRef>,
}

/// Compile one Bril instruction into CLIF.
fn compile_inst(inst: &bril::Instruction, builder: &mut FunctionBuilder, env: &CompileEnv) {
    match inst {
        bril::Instruction::Constant {
            dest,
            op: _,
            const_type: _,
            value,
        } => {
            let val = match value {
                bril::Literal::Int(i) => builder.ins().iconst(ir::types::I64, *i),
                bril::Literal::Bool(b) => builder.ins().bconst(ir::types::B1, *b),
            };
            builder.def_var(env.vars[dest], val);
        }
        bril::Instruction::Effect {
            args,
            funcs,
            labels,
            op,
        } => {
            match op {
                bril::EffectOps::Print => {
                    let mut first = true;
                    for arg in args {
                        // Separate printed values.
                        if first {
                            first = false;
                        } else {
                            builder.ins().call(env.rt_refs[RTFunc::PrintSep], &[]);
                        }

                        // Print each value according to its type.
                        let arg_val = builder.use_var(env.vars[arg]);
                        let print_func = match env.var_types[arg] {
                            bril::Type::Int => RTFunc::PrintInt,
                            bril::Type::Bool => RTFunc::PrintBool,
                        };
                        let print_ref = env.rt_refs[print_func];
                        builder.ins().call(print_ref, &[arg_val]);
                    }
                    builder.ins().call(env.rt_refs[RTFunc::PrintEnd], &[]);
                }
                bril::EffectOps::Jump => {
                    let block = env.blocks[&labels[0]];
                    builder.ins().jump(block, &[]);
                }
                bril::EffectOps::Branch => {
                    let arg = builder.use_var(env.vars[&args[0]]);
                    let true_block = env.blocks[&labels[0]];
                    let false_block = env.blocks[&labels[1]];
                    builder.ins().brnz(arg, true_block, &[]);
                    builder.ins().jump(false_block, &[]);
                }
                bril::EffectOps::Call => {
                    let func_ref = env.func_refs[&funcs[0]];
                    let arg_vals: Vec<ir::Value> = args
                        .iter()
                        .map(|arg| builder.use_var(env.vars[arg]))
                        .collect();
                    builder.ins().call(func_ref, &arg_vals);
                }
                bril::EffectOps::Return => {
                    if !args.is_empty() {
                        let arg = builder.use_var(env.vars[&args[0]]);
                        builder.ins().return_(&[arg]);
                    } else {
                        builder.ins().return_(&[]);
                    }
                }
                bril::EffectOps::Nop => {}
            }
        }
        bril::Instruction::Value {
            args,
            dest,
            funcs,
            labels: _,
            op,
            op_type,
        } => match op {
            bril::ValueOps::Add => {
                gen_binary(builder, &env.vars, args, dest, op_type, ir::Opcode::Iadd)
            }
            bril::ValueOps::Sub => {
                gen_binary(builder, &env.vars, args, dest, op_type, ir::Opcode::Isub)
            }
            bril::ValueOps::Mul => {
                gen_binary(builder, &env.vars, args, dest, op_type, ir::Opcode::Imul)
            }
            bril::ValueOps::Div => {
                gen_binary(builder, &env.vars, args, dest, op_type, ir::Opcode::Sdiv)
            }
            bril::ValueOps::Lt => gen_icmp(builder, &env.vars, args, dest, IntCC::SignedLessThan),
            bril::ValueOps::Le => {
                gen_icmp(builder, &env.vars, args, dest, IntCC::SignedLessThanOrEqual)
            }
            bril::ValueOps::Eq => gen_icmp(builder, &env.vars, args, dest, IntCC::Equal),
            bril::ValueOps::Ge => gen_icmp(
                builder,
                &env.vars,
                args,
                dest,
                IntCC::SignedGreaterThanOrEqual,
            ),
            bril::ValueOps::Gt => {
                gen_icmp(builder, &env.vars, args, dest, IntCC::SignedGreaterThan)
            }
            bril::ValueOps::And => {
                gen_binary(builder, &env.vars, args, dest, op_type, ir::Opcode::Band)
            }
            bril::ValueOps::Or => {
                gen_binary(builder, &env.vars, args, dest, op_type, ir::Opcode::Bor)
            }
            bril::ValueOps::Not => {
                let arg = builder.use_var(env.vars[&args[0]]);
                let res = builder.ins().bnot(arg);
                builder.def_var(env.vars[dest], res);
            }
            bril::ValueOps::Call => {
                let func_ref = env.func_refs[&funcs[0]];
                let arg_vals: Vec<ir::Value> = args
                    .iter()
                    .map(|arg| builder.use_var(env.vars[arg]))
                    .collect();
                let inst = builder.ins().call(func_ref, &arg_vals);
                let res = builder.inst_results(inst)[0];
                builder.def_var(env.vars[dest], res);
            }
            bril::ValueOps::Id => {
                let arg = builder.use_var(env.vars[&args[0]]);
                builder.def_var(env.vars[dest], arg);
            }
        },
    }
}

fn compile_body(insts: &[bril::Code], builder: &mut FunctionBuilder, env: &CompileEnv) {
    let mut terminated = false; // Entry block is open.
    for code in insts {
        match code {
            bril::Code::Instruction(inst) => {
                // If a normal instruction immediately follows a terminator, we need a new (anonymous) block.
                if terminated {
                    let block = builder.create_block();
                    builder.switch_to_block(block);
                    terminated = false;
                }

                // Compile one instruction.
                compile_inst(inst, builder, &env);

                if is_term(inst) {
                    terminated = true;
                }
            }
            bril::Code::Label { label } => {
                let new_block = env.blocks[label];

                // If the previous block was missing a terminator (fall-through), insert a
                // jump to the new block.
                if !terminated {
                    builder.ins().jump(new_block, &[]);
                }
                terminated = false;

                builder.switch_to_block(new_block);
            }
        }
    }

    // Implicit return in the last block.
    if !terminated {
        builder.ins().return_(&[]);
    }
}

impl<M: Module> Translator<M> {
    fn declare_func(&mut self, func: &bril::Function) -> cranelift_module::FuncId {
        // The Bril `main` function gets a different internal name, and we call it from a new
        // proper main function that gets argv/argc.
        let name = if func.name == "main" {
            "__bril_main"
        } else {
            &func.name
        };

        let sig = translate_sig(func);
        self.module
            .declare_function(name, cranelift_module::Linkage::Local, &sig)
            .unwrap()
    }

    fn enter_func(&mut self, func: &bril::Function, func_id: cranelift_module::FuncId) {
        let sig = translate_sig(func);
        self.context.func =
            ir::Function::with_name_signature(ir::ExternalName::user(0, func_id.as_u32()), sig);
    }

    fn finish_func(&mut self, func_id: cranelift_module::FuncId, dump: bool) {
        // Print the IR, if requested.
        if dump {
            println!("{}", self.context.func.display());
        }

        // Add to the module.
        self.module
            .define_function(func_id, &mut self.context)
            .unwrap();
        self.context.clear();
    }

    fn compile_func(&mut self, func: bril::Function) {
        let mut fn_builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut self.context.func, &mut fn_builder_ctx);

        // Declare runtime functions.
        let rt_refs = self
            .rt_funcs
            .map(|_, id| self.module.declare_func_in_func(id, builder.func));

        // Declare all variables (including for function parameters).
        let var_types = all_vars(&func);
        let mut vars = HashMap::<String, Variable>::new();
        for (i, (name, typ)) in var_types.iter().enumerate() {
            let var = Variable::new(i);
            builder.declare_var(var, translate_type(typ));
            vars.insert(name.to_string(), var);
        }

        // Create blocks for every label.
        let mut blocks = HashMap::<String, ir::Block>::new();
        for code in &func.instrs {
            if let bril::Code::Label { label } = code {
                let block = builder.create_block();
                blocks.insert(label.to_string(), block);
            }
        }

        // "Import" all the functions we may need to call.
        // TODO We could do this only for the functions we actually use...
        let func_refs: HashMap<String, ir::FuncRef> = self
            .funcs
            .iter()
            .map(|(name, id)| {
                (
                    name.to_owned(),
                    self.module.declare_func_in_func(*id, builder.func),
                )
            })
            .collect();

        // Cloning this map is not so great, but I am pretty bad at Rust!
        let var_types = var_types
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let env = CompileEnv {
            vars,
            var_types,
            rt_refs,
            blocks,
            func_refs,
        };

        // Define variables for function arguments in the entry block.
        let entry_block = builder.create_block();
        builder.switch_to_block(entry_block);
        builder.append_block_params_for_function_params(entry_block);
        for (i, arg) in func.args.iter().enumerate() {
            let param = builder.block_params(entry_block)[i];
            builder.def_var(env.vars[&arg.name], param);
        }

        // Insert instructions.
        compile_body(&func.instrs, &mut builder, &env);

        builder.seal_all_blocks();
        builder.finalize();
    }

    /// Generate a proper `main` function that calls the Bril `main` function.
    fn add_main(&mut self, args: &[bril::Argument], dump: bool) {
        // Declare `main` with argc/argv parameters.
        let sig = ir::Signature {
            params: vec![
                ir::AbiParam::new(self.pointer_type),
                ir::AbiParam::new(self.pointer_type),
            ],
            returns: vec![ir::AbiParam::new(self.pointer_type)],
            call_conv: isa::CallConv::SystemV,
        };
        let main_id = self
            .module
            .declare_function("main", cranelift_module::Linkage::Export, &sig)
            .unwrap();

        self.context.func =
            ir::Function::with_name_signature(ir::ExternalName::user(0, main_id.as_u32()), sig);

        // Declare `main`-specific setup runtime functions.
        let rt_setup_refs: EnumMap<RTSetupFunc, ir::FuncRef> = enum_map! {
            rt_setup_func => {
                let func_id = self
                    .module
                    .declare_function(
                        rt_setup_func.name(),
                        cranelift_module::Linkage::Import,
                        &rt_setup_func.sig(self.pointer_type),
                    )
                    .unwrap();
                self
                    .module
                    .declare_func_in_func(func_id, &mut self.context.func)
            }
        };

        let mut fn_builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut self.context.func, &mut fn_builder_ctx);

        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.seal_block(block);
        builder.append_block_params_for_function_params(block);

        // Parse each argument.
        let argv_arg = builder.block_params(block)[1]; // argc, argv
        let arg_vals: Vec<ir::Value> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let parse_ref = rt_setup_refs[match arg.arg_type {
                    bril::Type::Int => RTSetupFunc::ParseInt,
                    bril::Type::Bool => RTSetupFunc::ParseBool,
                }];
                let idx_arg = builder.ins().iconst(ir::types::I64, (i + 1) as i64); // skip argv[0]
                let inst = builder.ins().call(parse_ref, &[argv_arg, idx_arg]);
                builder.inst_results(inst)[0]
            })
            .collect();

        // Call the "real" main function.
        let real_main_id = self.funcs["main"];
        let real_main_ref = self.module.declare_func_in_func(real_main_id, builder.func);
        builder.ins().call(real_main_ref, &arg_vals);

        let zero = builder.ins().iconst(self.pointer_type, 0);
        builder.ins().return_(&[zero]);
        builder.finalize();

        // Add to the module.
        if dump {
            println!("{}", self.context.func.display());
        }
        self.module
            .define_function(main_id, &mut self.context)
            .unwrap();
        self.context.clear();
    }

    fn compile_prog(&mut self, prog: bril::Program, dump: bool, wrap_main: bool) {
        // Declare all functions.
        for func in &prog.functions {
            let id = self.declare_func(func);
            self.funcs.insert(func.name.to_owned(), id);
        }

        // Define all functions.
        for func in prog.functions {
            // If it's main, (maybe) wrap it in an entry function.
            if wrap_main && func.name == "main" {
                self.add_main(&func.args, dump);
            }

            // Compile every function.
            let id = self.funcs[&func.name];
            self.enter_func(&func, id);
            self.compile_func(func);
            self.finish_func(id, dump);
        }
    }
}

#[derive(FromArgs)]
#[argh(description = "Bril compiler")]
struct Args {
    #[argh(switch, short = 'j', description = "JIT and run")]
    jit: bool,

    #[argh(option, short = 't', description = "target triple")]
    target: Option<String>,

    #[argh(
        option,
        short = 'o',
        description = "output file",
        default = "String::from(\"bril.o\")"
    )]
    output: String,

    #[argh(switch, short = 'd', description = "dump CLIF IR")]
    dump_ir: bool,

    #[argh(switch, short = 'v', description = "verbose logging")]
    verbose: bool,

    #[argh(
        option,
        short = 'O',
        description = "optimization level (none, speed, or speed_and_size)",
        default = "String::from(\"none\")"
    )]
    opt_level: String,
}

fn main() {
    let args: Args = argh::from_env();

    // Set up logging.
    simplelog::TermLogger::init(
        if args.verbose {
            simplelog::LevelFilter::Debug
        } else {
            simplelog::LevelFilter::Warn
        },
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .unwrap();

    // Load the Bril program from stdin.
    let prog = bril::load_program();

    if args.jit {
        let mut trans = Translator::<JITModule>::new();
        trans.compile_prog(prog, args.dump_ir, false);
        trans.compile();
    } else {
        let mut trans = Translator::<ObjectModule>::new(args.target, &args.opt_level);
        trans.compile_prog(prog, args.dump_ir, true);
        trans.emit(&args.output);
    }
}
