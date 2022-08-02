import "@moonbeam-network/api-augment";
import { ApiDecoration } from "@polkadot/api/types";
import chalk from "chalk";
import { expect } from "chai";
import { printTokens } from "../util/logging";
import { describeSmokeSuite } from "../util/setup-smoke-tests";

// TEMPLATE: Remove useless types at the end
import type { PalletProxyProxyDefinition } from "@polkadot/types/lookup";

// TEMPLATE: Replace debug name
const debug = require("debug")("smoke:randomness");

const wssUrl = process.env.WSS_URL || null;
const relayWssUrl = process.env.RELAY_WSS_URL || null;

describeSmokeSuite(`Verify number of proxies per account`, { wssUrl, relayWssUrl }, (context) => {

  let atBlockNumber: number = 0;
  let apiAt: ApiDecoration<"promise"> = null;

  before("Setup API", async function () {
    atBlockNumber = process.env.BLOCK_NUMBER
      ? parseInt(process.env.BLOCK_NUMBER)
      : (await context.polkadotApi.rpc.chain.getHeader()).number.toNumber();
    apiAt = await context.polkadotApi.at(
      await context.polkadotApi.rpc.chain.getBlockHash(atBlockNumber)
    );
  });

  it("should have fewer Requests than RequestCount", async function () {
    this.timeout(10000);

    const requestCount = (await apiAt.query.randomness.requestCount() as any).toNumber();
    const numOutstandingRequests = (await apiAt.query.randomness.requests.entries()).length;

    expect(numOutstandingRequests).to.be.lessThanOrEqual(requestCount);
  });

  it("should have updated VRF output", async function () {
    this.timeout(10000);

    // we skip on if we aren't past the first block yet
    const notFirstBlock = (await apiAt.query.randomness.notFirstBlock() as any).isSome;
    if (notFirstBlock) {

      expect(atBlockNumber).to.be.greaterThan(0); // should be true if notFirstBlock
      const apiAtPrev = await context.polkadotApi.at(
        await context.polkadotApi.rpc.chain.getBlockHash(atBlockNumber - 1)
      );

      const currentOutput = await apiAt.query.randomness.localVrfOutput();
      const previousOutput = await apiAtPrev.query.randomness.localVrfOutput();

      expect(currentOutput.eq(previousOutput)).to.be.false;
    }
  });
});

