use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;
use std::fmt::Debug;

/// Object-safe trait that enables injecting extra precompiles when using
/// `anvil` as a library.
pub trait PrecompileFactory: Send + Sync + Unpin + Debug {
    /// Returns a set of precompiles to extend the EVM with.
    fn precompiles(&self) -> Vec<(Address, DynPrecompile)>;

    /// Installs precompiles into the given map. Default implementation
    /// calls [`Self::precompiles`] and extends the map.
    ///
    /// Implementors can override this for finer-grained control — for
    /// example, registering a `set_precompile_lookup` for dynamic or
    /// prefix-based address dispatch (the pattern Base's B-20 token
    /// precompile uses to claim every address with the `0xb2` prefix).
    fn install(&self, precompiles: &mut PrecompilesMap) {
        precompiles.extend_precompiles(self.precompiles());
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use crate::PrecompileFactory;
    use alloy_evm::{
        EthEvm, Evm, EvmEnv,
        eth::EthEvmContext,
        precompiles::{DynPrecompile, PrecompilesMap},
    };
    use alloy_op_evm::{OpEvm, OpTx};
    use alloy_primitives::{Address, Bytes, TxKind, U256, address};
    use foundry_evm::core::either_evm::EitherEvm;
    use foundry_evm_networks::NetworkConfigs;
    use itertools::Itertools;
    use op_revm::{
        DefaultOp, L1BlockInfo, OpBuilder, OpContext, OpSpecId, OpTransaction,
        precompiles::OpPrecompiles,
    };
    use revm::{
        MainBuilder, MainContext,
        context::{CfgEnv, TxEnv},
        database::{EmptyDB, EmptyDBTyped},
        handler::EthPrecompiles,
        inspector::NoOpInspector,
        precompile::{PrecompileOutput, PrecompileSpecId, Precompiles},
        primitives::hardfork::SpecId,
    };

    // A precompile activated in the `Prague` spec (BLS12-381 G2 map).
    const ETH_PRAGUE_PRECOMPILE: Address = address!("0x0000000000000000000000000000000000000011");

    // A precompile activated in the `Osaka` spec (EIP-7951).
    const ETH_OSAKA_PRECOMPILE: Address = address!("0x0000000000000000000000000000000000000100");

    // A precompile activated in the `Isthmus` spec.
    const OP_ISTHMUS_PRECOMPILE: Address = address!("0x0000000000000000000000000000000000000100");

    // A custom precompile address and payload for testing.
    const PRECOMPILE_ADDR: Address = address!("0x0000000000000000000000000000000000000071");
    const PAYLOAD: &[u8] = &[0xde, 0xad, 0xbe, 0xef];

    #[derive(Debug)]
    struct CustomPrecompileFactory;

    impl PrecompileFactory for CustomPrecompileFactory {
        fn precompiles(&self) -> Vec<(Address, DynPrecompile)> {
            use alloy_evm::precompiles::PrecompileInput;
            vec![(
                PRECOMPILE_ADDR,
                DynPrecompile::from(|input: PrecompileInput<'_>| {
                    Ok(PrecompileOutput::new(0, Bytes::copy_from_slice(input.data), 0))
                }),
            )]
        }
    }

    /// Creates a new EVM instance with the custom precompile factory.
    fn create_eth_evm(
        spec: SpecId,
    ) -> (foundry_evm::Env, EitherEvm<EmptyDBTyped<Infallible>, NoOpInspector, PrecompilesMap>)
    {
        let eth_env = foundry_evm::Env {
            evm_env: EvmEnv { block_env: Default::default(), cfg_env: CfgEnv::new_with_spec(spec) },
            tx: TxEnv {
                kind: TxKind::Call(PRECOMPILE_ADDR),
                data: PAYLOAD.into(),
                ..Default::default()
            },
        };

        let eth_precompiles = EthPrecompiles {
            precompiles: Precompiles::new(PrecompileSpecId::from_spec_id(spec)),
            spec,
        }
        .precompiles;
        let eth_evm = EitherEvm::Eth(EthEvm::new(
            EthEvmContext::<EmptyDB>::mainnet()
                .with_block(eth_env.evm_env.block_env.clone())
                .with_cfg(eth_env.evm_env.cfg_env.clone())
                .with_tx(eth_env.tx.clone())
                .with_db(EmptyDB::default())
                .build_mainnet_with_inspector(NoOpInspector {})
                .with_precompiles(PrecompilesMap::from_static(eth_precompiles)),
            true,
        ));

        (eth_env, eth_evm)
    }

    /// Creates a new OP EVM instance with the custom precompile factory.
    fn create_op_evm(
        spec: SpecId,
        op_spec: OpSpecId,
    ) -> (
        crate::eth::backend::env::Env,
        EitherEvm<EmptyDBTyped<Infallible>, NoOpInspector, PrecompilesMap>,
    ) {
        let op_env = crate::eth::backend::env::Env {
            evm_env: EvmEnv { block_env: Default::default(), cfg_env: CfgEnv::new_with_spec(spec) },
            tx: OpTransaction::<TxEnv> {
                base: TxEnv {
                    kind: TxKind::Call(PRECOMPILE_ADDR),
                    data: PAYLOAD.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            networks: NetworkConfigs::with_optimism(),
        };

        let mut chain = L1BlockInfo::default();

        if op_spec == OpSpecId::ISTHMUS {
            chain.operator_fee_constant = Some(U256::from(0));
            chain.operator_fee_scalar = Some(U256::from(0));
        }

        let op_cfg: CfgEnv<OpSpecId> = CfgEnv::new_with_spec(op_spec);
        let op_precompiles = OpPrecompiles::new_with_spec(op_cfg.spec).precompiles();
        let op_evm = EitherEvm::Op(OpEvm::new(
            OpContext::<EmptyDB>::op()
                .with_tx(OpTx(op_env.tx.clone()))
                .with_cfg(op_cfg.clone())
                .with_chain(chain)
                .with_db(EmptyDB::default())
                .with_block(op_env.evm_env.block_env.clone())
                .build_op_with_inspector(NoOpInspector {})
                .with_precompiles(PrecompilesMap::from_static(op_precompiles)),
            true,
        ));

        (op_env, op_evm)
    }

    #[test]
    fn build_eth_evm_with_extra_precompiles_osaka_spec() {
        let (env, mut evm) = create_eth_evm(SpecId::OSAKA);

        // Check that the Osaka precompile IS present when using the Osaka spec.
        assert!(evm.precompiles().addresses().contains(&ETH_OSAKA_PRECOMPILE));

        // Check that the Prague precompile IS present when using the Osaka spec.
        assert!(evm.precompiles().addresses().contains(&ETH_PRAGUE_PRECOMPILE));

        assert!(!evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        evm.precompiles_mut().extend_precompiles(CustomPrecompileFactory.precompiles());

        assert!(evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        let result = match &mut evm {
            EitherEvm::Eth(eth_evm) => eth_evm.transact(env.tx).unwrap(),
            _ => unreachable!(),
        };

        assert!(result.result.is_success());
        assert_eq!(result.result.output(), Some(&PAYLOAD.into()));
    }

    #[test]
    fn build_eth_evm_with_extra_precompiles_london_spec() {
        let (env, mut evm) = create_eth_evm(SpecId::LONDON);

        // Check that the Osaka precompile IS NOT present when using the London spec.
        assert!(!evm.precompiles().addresses().contains(&ETH_OSAKA_PRECOMPILE));

        // Check that the Prague precompile IS NOT present when using the London spec.
        assert!(!evm.precompiles().addresses().contains(&ETH_PRAGUE_PRECOMPILE));

        assert!(!evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        evm.precompiles_mut().extend_precompiles(CustomPrecompileFactory.precompiles());

        assert!(evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        let result = match &mut evm {
            EitherEvm::Eth(eth_evm) => eth_evm.transact(env.tx).unwrap(),
            _ => unreachable!(),
        };

        assert!(result.result.is_success());
        assert_eq!(result.result.output(), Some(&PAYLOAD.into()));
    }

    #[test]
    fn build_eth_evm_with_extra_precompiles_prague_spec() {
        let (env, mut evm) = create_eth_evm(SpecId::PRAGUE);

        // Check that the Osaka precompile IS NOT present when using the Prague spec.
        assert!(!evm.precompiles().addresses().contains(&ETH_OSAKA_PRECOMPILE));

        // Check that the Prague precompile IS present when using the Prague spec.
        assert!(evm.precompiles().addresses().contains(&ETH_PRAGUE_PRECOMPILE));

        assert!(!evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        evm.precompiles_mut().extend_precompiles(CustomPrecompileFactory.precompiles());

        assert!(evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        let result = match &mut evm {
            EitherEvm::Eth(eth_evm) => eth_evm.transact(env.tx).unwrap(),
            _ => unreachable!(),
        };

        assert!(result.result.is_success());
        assert_eq!(result.result.output(), Some(&PAYLOAD.into()));
    }

    #[test]
    fn build_op_evm_with_extra_precompiles_isthmus_spec() {
        let (env, mut evm) = create_op_evm(SpecId::OSAKA, OpSpecId::ISTHMUS);

        // Check that the Isthmus precompile IS present when using the Isthmus spec.
        assert!(evm.precompiles().addresses().contains(&OP_ISTHMUS_PRECOMPILE));

        // Check that the Prague precompile IS present when using the Isthmus spec.
        assert!(evm.precompiles().addresses().contains(&ETH_PRAGUE_PRECOMPILE));

        assert!(!evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        evm.precompiles_mut().extend_precompiles(CustomPrecompileFactory.precompiles());

        assert!(evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        let result = match &mut evm {
            EitherEvm::Op(op_evm) => op_evm.transact(OpTx(env.tx)).unwrap(),
            _ => unreachable!(),
        };

        assert!(result.result.is_success());
        assert_eq!(result.result.output(), Some(&PAYLOAD.into()));
    }

    #[test]
    fn build_op_evm_with_extra_precompiles_bedrock_spec() {
        let (env, mut evm) = create_op_evm(SpecId::OSAKA, OpSpecId::BEDROCK);

        // Check that the Isthmus precompile IS NOT present when using the `OpSpecId::BEDROCK` spec.
        assert!(!evm.precompiles().addresses().contains(&OP_ISTHMUS_PRECOMPILE));

        // Check that the Prague precompile IS NOT present when using the `OpSpecId::BEDROCK` spec.
        assert!(!evm.precompiles().addresses().contains(&ETH_PRAGUE_PRECOMPILE));

        assert!(!evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        evm.precompiles_mut().extend_precompiles(CustomPrecompileFactory.precompiles());

        assert!(evm.precompiles().addresses().contains(&PRECOMPILE_ADDR));

        let result = match &mut evm {
            EitherEvm::Op(op_evm) => op_evm.transact(OpTx(env.tx)).unwrap(),
            _ => unreachable!(),
        };

        assert!(result.result.is_success());
        assert_eq!(result.result.output(), Some(&PAYLOAD.into()));
    }
}
