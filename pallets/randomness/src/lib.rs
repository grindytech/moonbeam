// Copyright 2019-2022 PureStake Inc.
// This file is part of Moonbeam.

// Moonbeam is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Moonbeam is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Moonbeam.  If not, see <http://www.gnu.org/licenses/>.

//! Randomness pallet

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::pallet;

pub use pallet::*;

#[cfg(any(test, feature = "runtime-benchmarks"))]
mod benchmarks;
pub mod types;
pub mod vrf;
pub use types::*;
pub mod weights;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

/// Read babe randomness info from the relay chain state proof
pub trait GetBabeData<EpochIndex, Randomness> {
	fn get_epoch_index() -> EpochIndex;
	fn get_epoch_randomness() -> Randomness;
}

#[pallet]
pub mod pallet {
	use super::*;
	use frame_support::traits::{Currency, ExistenceRequirement::KeepAlive};
	use frame_support::{pallet_prelude::*, PalletId};
	use frame_system::pallet_prelude::*;
	use nimbus_primitives::NimbusId;
	use pallet_evm::AddressMapping;
	use session_keys_primitives::{InherentError, KeysLookup, VrfId, INHERENT_IDENTIFIER};
	use sp_core::{H160, H256};
	use sp_runtime::traits::{AccountIdConversion, Saturating};
	use sp_std::{convert::TryInto, vec::Vec};

	/// The Randomness's pallet id
	pub const PALLET_ID: PalletId = PalletId(*b"moonrand");

	/// Request identifier, unique per request for randomness
	pub type RequestId = u64;

	pub type BalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(PhantomData<T>);

	/// Configuration trait of this pallet.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Overarching event type
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		/// Address mapping to convert from H160 to AccountId
		type AddressMapping: AddressMapping<Self::AccountId>;
		/// Currency in which the security deposit will be taken.
		type Currency: Currency<Self::AccountId>;
		/// Get the BABE data from the runtime
		type BabeDataGetter: GetBabeData<u64, Option<Self::Hash>>;
		/// Takes NimbusId to return VrfId
		type VrfKeyLookup: KeysLookup<NimbusId, VrfId>;
		#[pallet::constant]
		/// The amount that should be taken as a security deposit when requesting randomness.
		type Deposit: Get<BalanceOf<Self>>;
		#[pallet::constant]
		/// Maximum number of random words that can be requested per request
		type MaxRandomWords: Get<u8>;
		#[pallet::constant]
		/// Local per-block VRF requests must be at least this many blocks after the block in which
		/// they were requested
		type MinBlockDelay: Get<Self::BlockNumber>;
		#[pallet::constant]
		/// Local per-block VRF requests must be at most this many blocks after the block in which
		/// they were requested
		type MaxBlockDelay: Get<Self::BlockNumber>;
		#[pallet::constant]
		/// Local requests expire and can be purged from storage after this many blocks/epochs
		type BlockExpirationDelay: Get<Self::BlockNumber>;
		#[pallet::constant]
		/// Babe requests expire and can be purged from storage after this many blocks/epochs
		type EpochExpirationDelay: Get<u64>;
	}

	#[pallet::error]
	pub enum Error<T> {
		RequestCounterOverflowed,
		RequestFeeOverflowed,
		MustRequestAtLeastOneWord,
		CannotRequestMoreWordsThanMax,
		CannotRequestRandomnessAfterMaxDelay,
		CannotRequestRandomnessBeforeMinDelay,
		RequestDNE,
		RequestCannotYetBeFulfilled,
		OnlyRequesterCanIncreaseFee,
		RequestHasNotExpired,
		RandomnessResultDNE,
		RandomnessResultNotFilled,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		RandomnessRequestedBabeEpoch {
			id: RequestId,
			refund_address: H160,
			contract_address: H160,
			fee: BalanceOf<T>,
			gas_limit: u64,
			num_words: u8,
			salt: H256,
			earliest_epoch: u64,
		},
		RandomnessRequestedLocal {
			id: RequestId,
			refund_address: H160,
			contract_address: H160,
			fee: BalanceOf<T>,
			gas_limit: u64,
			num_words: u8,
			salt: H256,
			earliest_block: T::BlockNumber,
		},
		RequestFulfilled {
			id: RequestId,
		},
		RequestFeeIncreased {
			id: RequestId,
			new_fee: BalanceOf<T>,
		},
		RequestExpirationExecuted {
			id: RequestId,
		},
	}

	#[pallet::storage]
	#[pallet::getter(fn requests)]
	/// Randomness requests not yet fulfilled or purged
	pub type Requests<T: Config> = StorageMap<_, Twox64Concat, RequestId, RequestState<T>>;

	#[pallet::storage]
	#[pallet::getter(fn request_count)]
	/// Number of randomness requests made so far, used to generate the next request's uid
	pub type RequestCount<T: Config> = StorageValue<_, RequestId, ValueQuery>;

	/// Current local per-block VRF randomness
	/// Set in `on_initialize`
	#[pallet::storage]
	#[pallet::getter(fn local_vrf_output)]
	pub type LocalVrfOutput<T: Config> = StorageValue<_, Option<T::Hash>, ValueQuery>;

	/// Relay epoch
	#[pallet::storage]
	#[pallet::getter(fn relay_epoch)]
	pub(crate) type RelayEpoch<T: Config> = StorageValue<_, u64, ValueQuery>;

	/// Ensures the mandatory inherent was included in the block
	#[pallet::storage]
	#[pallet::getter(fn inherent_included)]
	pub(crate) type InherentIncluded<T: Config> = StorageValue<_, ()>;

	/// Records whether this is the first block (genesis or runtime upgrade)
	#[pallet::storage]
	#[pallet::getter(fn not_first_block)]
	pub type NotFirstBlock<T: Config> = StorageValue<_, ()>;

	/// Snapshot of randomness to fulfill all requests that are for the same raw randomness
	/// Removed once $value.request_count == 0
	#[pallet::storage]
	#[pallet::getter(fn randomness_results)]
	pub(crate) type RandomnessResults<T: Config> =
		StorageMap<_, Twox64Concat, RequestType<T>, RandomnessResult<T::Hash>>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Populates the `RandomnessResults` that are due this block with the raw values
		// This inherent is a workaround to run code in every block after
		// `ParachainSystem::set_validation_data` but before all extrinsics.
		// the relevant storage item to validate the VRF output in this pallet's `on_initialize`
		// This should go into on_post_inherents when it is ready
		// https://github.com/paritytech/substrate/pull/10128
		// TODO: weight
		#[pallet::weight((10_000, DispatchClass::Mandatory))]
		pub fn set_babe_randomness_results(origin: OriginFor<T>) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			let last_relay_epoch_index = <RelayEpoch<T>>::get();
			// populate the `RandomnessResults` for BABE epoch randomness (1 and 2 ago)
			let relay_epoch_index = T::BabeDataGetter::get_epoch_index();
			if relay_epoch_index > last_relay_epoch_index {
				let babe_one_epoch_ago_this_block = RequestType::BabeEpoch(relay_epoch_index);
				if let Some(mut results) =
					<RandomnessResults<T>>::get(&babe_one_epoch_ago_this_block)
				{
					if let Some(randomness) = T::BabeDataGetter::get_epoch_randomness() {
						results.randomness = Some(randomness);
						<RandomnessResults<T>>::insert(babe_one_epoch_ago_this_block, results);
					} else {
						log::warn!(
							"Failed to fill BABE epoch randomness results \
							REQUIRE HOTFIX TO FILL EPOCH RANDOMNESS RESULTS FOR EPOCH {:?}",
							relay_epoch_index
						);
					}
				}
			}
			<RelayEpoch<T>>::put(relay_epoch_index);
			<InherentIncluded<T>>::put(());

			Ok(Pays::No.into())
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = InherentError;
		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn is_inherent_required(_: &InherentData) -> Result<Option<Self::Error>, Self::Error> {
			// Return Ok(Some(_)) unconditionally because this inherent is required in every block
			// If it is not found, throw a VrfInherentRequired error.
			Ok(Some(InherentError::Other(
				sp_runtime::RuntimeString::Borrowed(
					"Inherent required to set babe randomness results",
				),
			)))
		}

		// The empty-payload inherent extrinsic.
		fn create_inherent(_data: &InherentData) -> Option<Self::Call> {
			Some(Call::set_babe_randomness_results {})
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::set_babe_randomness_results { .. })
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		// Set this block's randomness using the VRF output
		fn on_initialize(_now: BlockNumberFor<T>) -> Weight {
			// Set and validate VRF output
			vrf::set_output::<T>()
		}
		// Set next block's VRF input in storage
		fn on_finalize(_now: BlockNumberFor<T>) {
			// Panics if set_babe_randomness_results inherent was not included
			assert!(
				<InherentIncluded<T>>::take().is_some(),
				"Mandatory randomness inherent not included; InherentIncluded storage item is empty"
			);
		}
	}

	// Utility function
	impl<T: Config> Pallet<T> {
		pub fn account_id() -> T::AccountId {
			PALLET_ID.into_account_truncating()
		}
		pub fn total_locked() -> BalanceOf<T> {
			// free balance is usable balance for the pallet account as it is not controlled
			// by anyone so will never be locked
			T::Currency::free_balance(&Self::account_id())
		}
		pub(crate) fn concat_and_hash(a: T::Hash, b: H256, index: u8) -> Vec<[u8; 32]> {
			let mut output: Vec<[u8; 32]> = Vec::new();
			let mut s = Vec::new();
			for i in 0u8..index {
				s.extend_from_slice(a.as_ref());
				s.extend_from_slice(b.as_ref());
				s.extend_from_slice(&[i]);
				output.push(sp_io::hashing::blake2_256(&s));
				s.clear();
			}
			output
		}
	}

	// Public functions for precompile usage only
	impl<T: Config> Pallet<T> {
		pub fn request_randomness(
			request: Request<BalanceOf<T>, RequestType<T>>,
		) -> Result<RequestId, sp_runtime::DispatchError> {
			let request = RequestState::new(request.into())?;
			let (fee, contract_address, info) = (
				request.request.fee,
				request.request.contract_address,
				request.request.info.clone(),
			);
			let total_to_reserve = T::Deposit::get().saturating_add(fee);
			let contract_address = T::AddressMapping::into_account_id(contract_address);
			// get new request ID
			let request_id = <RequestCount<T>>::get();
			let next_id = request_id
				.checked_add(1u64)
				.ok_or(Error::<T>::RequestCounterOverflowed)?;
			// send deposit to reserve account
			T::Currency::transfer(
				&contract_address,
				&Self::account_id(),
				total_to_reserve,
				KeepAlive,
			)?;
			let info_key: RequestType<T> = info.into();
			if let Some(existing_randomness_snapshot) = <RandomnessResults<T>>::take(&info_key) {
				<RandomnessResults<T>>::insert(
					&info_key,
					existing_randomness_snapshot.increment_request_count(),
				);
			} else {
				<RandomnessResults<T>>::insert(&info_key, RandomnessResult::new());
			}
			// insert request
			<RequestCount<T>>::put(next_id);
			request.request.emit_randomness_requested_event(request_id);
			<Requests<T>>::insert(request_id, request);
			Ok(request_id)
		}
		/// Prepare fulfillment
		/// Returns all arguments needed for fulfillment
		pub fn prepare_fulfillment(id: RequestId) -> Result<FulfillArgs<T>, DispatchError> {
			<Requests<T>>::get(id)
				.ok_or(Error::<T>::RequestDNE)?
				.prepare_fulfill()
		}
		/// Finish fulfillment
		/// Caller MUST ensure `id` corresponds to `request` or there will be side effects
		pub fn finish_fulfillment(
			id: RequestId,
			request: Request<BalanceOf<T>, RequestInfo<T>>,
			deposit: BalanceOf<T>,
			caller: &H160,
			cost_of_execution: BalanceOf<T>,
		) {
			request.finish_fulfill(deposit, caller, cost_of_execution);
			let info_key: RequestType<T> = request.info.into();
			if let Some(result) = RandomnessResults::<T>::take(&info_key) {
				if let Some(new_result) = result.decrement_request_count() {
					RandomnessResults::<T>::insert(&info_key, new_result);
				} // else RandomnessResult is removed from storage
			}
			<Requests<T>>::remove(id);
			Self::deposit_event(Event::RequestFulfilled { id });
		}
		/// Increase fee associated with request
		pub fn increase_request_fee(
			caller: &H160,
			id: RequestId,
			fee_increase: BalanceOf<T>,
		) -> DispatchResult {
			let mut request = <Requests<T>>::get(id).ok_or(Error::<T>::RequestDNE)?;
			// Increase randomness request fee
			let new_fee = request.increase_fee(caller, fee_increase)?;
			<Requests<T>>::insert(id, request);
			Self::deposit_event(Event::RequestFeeIncreased { id, new_fee });
			Ok(())
		}
		/// Execute request expiration
		/// transfers fee to caller && purges request iff it has expired
		/// does NOT try to fulfill the request
		pub fn execute_request_expiration(caller: &H160, id: RequestId) -> DispatchResult {
			let request = <Requests<T>>::get(id).ok_or(Error::<T>::RequestDNE)?;
			let caller = T::AddressMapping::into_account_id(caller.clone());
			request.execute_expiration(&caller)?;
			let info_key: RequestType<T> = request.request.info.into();
			if let Some(result) = RandomnessResults::<T>::take(&info_key) {
				if let Some(new_result) = result.decrement_request_count() {
					RandomnessResults::<T>::insert(&info_key, new_result);
				} // else RandomnessResult is removed from storage
			}
			<Requests<T>>::remove(id);
			Self::deposit_event(Event::RequestExpirationExecuted { id });
			Ok(())
		}
	}
}
