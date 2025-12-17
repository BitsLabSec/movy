use movy_types::input::{MoveSequenceCall, SequenceArgument};

fn remap_arg_with_map(arg: &SequenceArgument, mapping: &[Option<u16>]) -> Option<SequenceArgument> {
    match arg {
        SequenceArgument::Result(i) => mapping
            .get(*i as usize)
            .and_then(|new| new.map(SequenceArgument::Result)),
        SequenceArgument::NestedResult(i, j) => mapping
            .get(*i as usize)
            .and_then(|new| new.map(|n| SequenceArgument::NestedResult(n, *j))),
        _ => Some(arg.clone()),
    }
}

pub fn remap_arg(arg: &SequenceArgument, mapping: &[u16]) -> SequenceArgument {
    match arg {
        SequenceArgument::Result(i) => {
            SequenceArgument::Result(*mapping.get(*i as usize).unwrap_or_else(|| {
                panic!("missing mapping for command index {}", i);
            }))
        }
        SequenceArgument::NestedResult(i, j) => SequenceArgument::NestedResult(
            *mapping.get(*i as usize).unwrap_or_else(|| {
                panic!("missing mapping for command index {}", i);
            }),
            *j,
        ),
        _ => arg.clone(),
    }
}

pub fn remap_command_with_map(
    cmd: &MoveSequenceCall,
    mapping: &[Option<u16>],
) -> Option<MoveSequenceCall> {
    let mut remap_args = |arg: &SequenceArgument| remap_arg_with_map(arg, mapping);
    match cmd {
        MoveSequenceCall::Call(movecall) => {
            let mut new_call = movecall.clone();
            new_call.arguments = movecall
                .arguments
                .iter()
                .map(&mut remap_args)
                .collect::<Option<Vec<_>>>()?;
            Some(MoveSequenceCall::Call(new_call))
        }
        MoveSequenceCall::TransferObjects(args, dst) => {
            let args = args
                .iter()
                .map(&mut remap_args)
                .collect::<Option<Vec<_>>>()?;
            let dst = remap_args(dst)?;
            Some(MoveSequenceCall::TransferObjects(args, dst))
        }
        MoveSequenceCall::SplitCoins(src, amts) => {
            let src = remap_args(src)?;
            let amts = amts
                .iter()
                .map(&mut remap_args)
                .collect::<Option<Vec<_>>>()?;
            Some(MoveSequenceCall::SplitCoins(src, amts))
        }
        MoveSequenceCall::MergeCoins(dst, srcs) => {
            let dst = remap_args(dst)?;
            let srcs = srcs
                .iter()
                .map(&mut remap_args)
                .collect::<Option<Vec<_>>>()?;
            Some(MoveSequenceCall::MergeCoins(dst, srcs))
        }
        MoveSequenceCall::MakeMoveVec(ty, args) => {
            let args = args
                .iter()
                .map(&mut remap_args)
                .collect::<Option<Vec<_>>>()?;
            Some(MoveSequenceCall::MakeMoveVec(ty.clone(), args))
        }
        MoveSequenceCall::Publish(mods, deps) => {
            Some(MoveSequenceCall::Publish(mods.clone(), deps.clone()))
        }
        MoveSequenceCall::Upgrade(mods, deps, package, ticket) => {
            let ticket = remap_args(ticket)?;
            Some(MoveSequenceCall::Upgrade(
                mods.clone(),
                deps.clone(),
                *package,
                ticket,
            ))
        }
    }
}

pub fn remap_command(cmd: &MoveSequenceCall, mapping: &[u16]) -> MoveSequenceCall {
    let mut remap_args = |arg: &SequenceArgument| remap_arg(arg, mapping);
    match cmd {
        MoveSequenceCall::Call(movecall) => {
            let mut new_call = movecall.clone();
            new_call.arguments = movecall.arguments.iter().map(&mut remap_args).collect();
            MoveSequenceCall::Call(new_call)
        }
        MoveSequenceCall::TransferObjects(args, dst) => MoveSequenceCall::TransferObjects(
            args.iter().map(&mut remap_args).collect(),
            remap_args(dst),
        ),
        MoveSequenceCall::SplitCoins(src, amts) => MoveSequenceCall::SplitCoins(
            remap_args(src),
            amts.iter().map(&mut remap_args).collect(),
        ),
        MoveSequenceCall::MergeCoins(dst, srcs) => MoveSequenceCall::MergeCoins(
            remap_args(dst),
            srcs.iter().map(&mut remap_args).collect(),
        ),
        MoveSequenceCall::MakeMoveVec(ty, args) => {
            MoveSequenceCall::MakeMoveVec(ty.clone(), args.iter().map(&mut remap_args).collect())
        }
        MoveSequenceCall::Publish(mods, deps) => {
            MoveSequenceCall::Publish(mods.clone(), deps.clone())
        }
        MoveSequenceCall::Upgrade(mods, deps, package, ticket) => {
            MoveSequenceCall::Upgrade(mods.clone(), deps.clone(), *package, remap_args(ticket))
        }
    }
}
