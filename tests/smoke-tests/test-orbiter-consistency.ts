import "@moonbeam-network/api-augment";
import { ApiDecoration } from "@polkadot/api/types";
import { bool, Option, u32 } from "@polkadot/types-codec";
import type {
  FrameSystemEventRecord,
  PalletMoonbeamOrbitersCollatorPoolInfo,
} from "@polkadot/types/lookup";
import type { AccountId20 } from "@polkadot/types/interfaces";

import { expect } from "chai";
import { describeSmokeSuite } from "../util/setup-smoke-tests";
import { StorageKey } from "@polkadot/types";
const debug = require("debug")("smoke:orbiter");

const wssUrl = process.env.WSS_URL || null;
const relayWssUrl = process.env.RELAY_WSS_URL || null;
// TODO: rotatePeriod is not exposed in metada yet, change that after RT1800
const rotatePeriod: number = process.env.ROTATE_PERIOD ? parseInt(process.env.ROTATE_PERIOD) : 3;

describeSmokeSuite(`Verify orbiters`, { wssUrl, relayWssUrl }, (context) => {
  let atBlockNumber: number = 0;
  let apiAt: ApiDecoration<"promise"> = null;
  let collatorsPools: [
    StorageKey<[AccountId20]>,
    Option<PalletMoonbeamOrbitersCollatorPoolInfo>
  ][] = null;
  let registeredOrbiters: [StorageKey<[AccountId20]>, Option<bool>][] = null;
  let counterForCollatorsPool: u32 = null;
  let currentRound: number = null;
  let orbiterPerRound: [StorageKey<[u32, AccountId20]>, Option<AccountId20>][] = null;
  let events: FrameSystemEventRecord[] = null;

  before("Setup api & retrieve data", async function () {
    const runtimeVersion = await context.polkadotApi.runtimeVersion.specVersion.toNumber();
    atBlockNumber = process.env.BLOCK_NUMBER
      ? parseInt(process.env.BLOCK_NUMBER)
      : (await context.polkadotApi.rpc.chain.getHeader()).number.toNumber();
    apiAt = await context.polkadotApi.at(
      await context.polkadotApi.rpc.chain.getBlockHash(atBlockNumber)
    );
    collatorsPools = await apiAt.query.moonbeamOrbiters.collatorsPool.entries();
    registeredOrbiters =
      runtimeVersion >= 1605 ? await apiAt.query.moonbeamOrbiters.registeredOrbiter.entries() : [];
    counterForCollatorsPool = await apiAt.query.moonbeamOrbiters.counterForCollatorsPool();
    currentRound = (await apiAt.query.parachainStaking.round()).current.toNumber();
    orbiterPerRound = await apiAt.query.moonbeamOrbiters.orbiterPerRound.entries();
    events = await apiAt.query.system.events();
  });

  it("should have reserved tokens", async function () {
    const reserves = await apiAt.query.balances.reserves.entries();
    const orbiterReserves = reserves
      .map((reserveSet) =>
        reserveSet[1].find((r) => r.id.toUtf8() == "orbi")
          ? `0x${reserveSet[0].toHex().slice(-40)}`
          : null
      )
      .filter((r) => !!r);

    const orbiterRegisteredAccounts = registeredOrbiters.map((o) => `0x${o[0].toHex().slice(-40)}`);

    for (const reservedAccount of orbiterReserves) {
      expect(
        orbiterRegisteredAccounts,
        `Account ${reservedAccount} has "orbi" reserve but is not orbiter.`
      ).to.include(reservedAccount);
    }

    for (const orbiterAccount of orbiterRegisteredAccounts) {
      expect(
        orbiterReserves,
        `Account ${orbiterAccount} is orbiter but doesn't have "orbi" reserve.`
      ).to.include(orbiterAccount);
    }
    debug(`Verified ${orbiterRegisteredAccounts.length} orbiter reserves`);
  });

  it("should be registered if in a pool", async function () {
    for (const orbiterPool of collatorsPools) {
      const collator = `0x${orbiterPool[0].toHex().slice(-40)}`;
      const pool = orbiterPool[1].unwrap();
      const orbiterRegisteredAccounts = registeredOrbiters.map(
        (o) => `0x${o[0].toHex().slice(-40)}`
      );
      if (pool.maybeCurrentOrbiter.isSome) {
        const selectedOrbiter = pool.maybeCurrentOrbiter.unwrap().accountId.toHex();
        const isRemoved = pool.maybeCurrentOrbiter.unwrap().removed.isTrue;
        const poolOrbiters = pool.orbiters.map((o) => o.toHex());

        if (isRemoved) {
          expect(
            poolOrbiters,
            `Selected orbiter ${selectedOrbiter} is removed but ` +
              `still in the pool ${collator} orbiters`
          ).to.not.include(selectedOrbiter);
        } else {
          expect(
            poolOrbiters,
            `Selected orbiter ${selectedOrbiter} is not in the pool ${collator} orbiters`
          ).to.include(selectedOrbiter);
        }

        expect(
          orbiterRegisteredAccounts,
          `Account ${selectedOrbiter} is in a pool but not registered`
        ).to.include(selectedOrbiter);
      }
    }

    debug(`Verified ${collatorsPools.length} orbiter pools`);
  });

  it("should not have more pool than the max allowed", async function () {
    expect(collatorsPools.length, `Orbiter pool is too big`).to.be.at.most(
      counterForCollatorsPool.toNumber()
    );

    debug(`Verified orbiter pools size`);
  });

  it("should have matching rewards", async function () {
    // Get parent collators
    const parentCollators = collatorsPools.map((o) => `0x${o[0].toHex().toUpperCase().slice(-40)}`);
    console.log(parentCollators);

    // Get collators rewards
    let collatorRewards = {};
    for (const { event, phase } of events) {
      if (
        phase.isInitialization &&
        event.section == "parachainStaking" &&
        event.method == "Rewarded"
      ) {
        const data = event.data.toHuman() as any;
        console.log(data.account);
        if (parentCollators.includes(data.account.toString())) {
          collatorRewards[data.account] = data.rewards;
        }
      }
    }

    console.log(collatorRewards);

    if (Object.keys(collatorRewards).length > 0) {
      // Compute expected reward for each orbiter
      const lastRotateRound = currentRound - (currentRound % rotatePeriod);
      let expectedOrbiterRewards = {};
      orbiterPerRound.forEach((o) => {
        let [round, collator] = o[0].args;
        let orbiter = o[1];

        if (round.toNumber() == lastRotateRound) {
          expectedOrbiterRewards[orbiter.unwrap().toHex()] = collatorRewards[collator.toHex()];
        }
      });

      // Verify orbiters rewards
      let countRewardedOrbiters = 0;
      for (const { event, phase } of events) {
        if (
          phase.isInitialization &&
          event.section == "MoonbeamOrbiters" &&
          event.method == "OrbiterRewarded"
        ) {
          countRewardedOrbiters += 1;
          const data = event.data.toHuman() as any;
          const orbiter = data.account;
          const rewards = data.rewards;

          expect(
            Object.keys(expectedOrbiterRewards),
            `Orbiter ${orbiter} has received unexpected rewards (expect 0).`
          ).to.include(orbiter);

          expect(
            expectedOrbiterRewards[orbiter],
            `Orbiter ${orbiter} rewards for round ${currentRound} doesn't match expectation.`
          ).to.equal(rewards);
        }
      }

      console.log(expectedOrbiterRewards);

      expect(
        countRewardedOrbiters,
        `The number of rewarded orbiters doesn't match expectation.`
      ).to.equal(Object.keys(expectedOrbiterRewards).length);
    }
  });
});
