//! Bithumb Dex moudule
//!
//! ##Overview
//! Decentralized exchange module in bitdex network, supports
//! create trading pairs in any supported currencies.

#![cfg_attr(not(feature = "std"), no_std)]

use byteorder::{ByteOrder, LittleEndian};

use frame_support::{
	decl_error, decl_event, decl_module, decl_storage, ensure,
  debug,
	traits::{Get, Happened},
	weights::constants::WEIGHT_PER_MICROS,
	Parameter,
};
use frame_support::storage::IterableStorageMap;

use frame_system::{self as system, ensure_signed};

use num_traits::FromPrimitive;

use orml_traits::{MultiCurrency, MultiCurrencyExtended};
use orml_utilities::with_transaction_result;
use primitives::{Balance, CurrencyId, Price, Rate, Ratio};

use sp_runtime::{
	traits::{
		AccountIdConversion, AtLeast32Bit, CheckedAdd, CheckedSub, MaybeSerializeDeserialize, Member,
		Saturating, UniqueSaturatedInto, Zero, One,
	},
	DispatchError, DispatchResult, FixedPointNumber, FixedPointOperand, ModuleId,
};

use sp_std::vec;
use sp_std::collections::btree_map;

mod simple_graph;

pub trait Trait: system::Trait {
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	/// Associate type for measuring liquidity contribution of specific trading
	/// pairs
	type Share: Parameter + Member + AtLeast32Bit + Default + Copy + MaybeSerializeDeserialize + FixedPointOperand;

	/// Currency for transfer currencies
	type Currency: MultiCurrencyExtended<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

	/// Trading fee rate
	type GetExchangeFee: Get<Rate>;

	/// The DEX's module id, keep all assets in DEX sub account.
	type ModuleId: Get<ModuleId>;

	/// Event handler which calls when add liquidity.
	type OnAddLiquidity: Happened<(Self::AccountId, CurrencyId, CurrencyId, Self::Share)>;

	/// Event handler which calls when remove liquidity.
  type OnRemoveLiquidity: Happened<(Self::AccountId, CurrencyId, CurrencyId, Self::Share)>;
}

pub type PairKey = u64;
pub type PoolInfo = (Balance, Balance);

decl_event!(
	pub enum Event<T> where
		<T as system::Trait>::AccountId,
		<T as Trait>::Share,
		Balance = Balance,
		CurrencyId = CurrencyId,
	{
		/// Add liquidity success. [who, currency_type, added_currency_amount, added_base_currency_amount, increment_share_amount]
		AddLiquidity(AccountId, CurrencyId, CurrencyId, Balance, Balance, Share),
		/// Withdraw liquidity from the trading pool success. [who, currency_type, withdrawn_currency_amount, withdrawn_base_currency_amount, burned_share_amount]
		WithdrawLiquidity(AccountId, CurrencyId, CurrencyId, Balance, Balance, Share),
		/// Use supply currency to swap target currency. [trader, supply_currency_type, supply_currency_amount, target_currency_type, target_currency_amount]
		Swap(AccountId, CurrencyId, CurrencyId, Balance, CurrencyId, Balance),
		/// Incentive reward rate updated. [currency_type, new_rate]
		LiquidityIncentiveRateUpdated(CurrencyId, CurrencyId, Rate),
		/// Incentive interest claimed. [who, currency_type, amount]
		IncentiveInterestClaimed(AccountId, CurrencyId, CurrencyId, Balance),
	}
);

decl_error! {
	/// Error for dex module.
	pub enum Error for Module<T: Trait> {
		/// Currency Pair not exists
		InvalidCurrencyPair,
		/// Not the tradable currency type
		CurrencyIdNotAllowed,
		/// Share amount is not enough
		ShareNotEnough,
		/// Share amount overflow
		SharesOverflow,
		/// The actual transaction price will be lower than the acceptable price
		UnacceptablePrice,
		/// The increment of liquidity is invalid
		InvalidLiquidityIncrement,
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as BithumbDex {
		/// Liquidity pool, which is the trading pair for specific currency type to base currency type.
		/// CurrencyType -> (OtherCurrencyAmount, BaseCurrencyAmount)
		LiquidityPool get(fn liquidity_pool): map hasher(blake2_128_concat) PairKey => (Balance, Balance);

		/// Total shares amount of liquidity pool specified by currency type
		/// CurrencyType -> TotalSharesAmount
		TotalShares get(fn total_shares): map hasher(blake2_128_concat) PairKey => T::Share;

		/// Shares records indexed by currency type and account id
		/// CurrencyType -> Owner -> ShareAmount
		Shares get(fn shares): double_map hasher(blake2_128_concat) PairKey, hasher(twox_64_concat) T::AccountId => T::Share;
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
    type Error = Error<T>;

		fn deposit_event() = default;

		/// Trading fee rate
		const GetExchangeFee: Rate = T::GetExchangeFee::get();

		/// The DEX's module id, keep all assets in DEX.
		const ModuleId: ModuleId = T::ModuleId::get();

		#[weight = 206 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(10, 9)]
		pub fn add_liquidity(
			origin,
			currency_id_first: CurrencyId,
			currency_id_second: CurrencyId,
			#[compact] max_first_currency_amount: Balance,
			#[compact] max_second_currency_amount: Balance,
		) {
			ensure!(currency_id_first != currency_id_second, Error::<T>::InvalidCurrencyPair);

			with_transaction_result(|| {
				let who = ensure_signed(origin)?;
        let pair_id = Self::get_pair_key(&currency_id_first, &currency_id_second);
				ensure!(
					LiquidityPool::contains_key(pair_id),
					Error::<T>::InvalidCurrencyPair,
				);

        //
        // normalize currency pair, smaller at the left side
        let (currency_id_left, currency_id_right,
             max_currency_amount_left, max_currency_amount_right) = if currency_id_first < currency_id_second {
          (currency_id_first, currency_id_second,
           max_first_currency_amount, max_second_currency_amount)
        } else {
          (currency_id_second, currency_id_first,
           max_second_currency_amount, max_first_currency_amount)
        };

        let total_shares = Self::total_shares(pair_id);
				let (left_currency_increment, right_currency_increment, share_increment): (Balance, Balance, T::Share) =
				if total_shares.is_zero() {
					// initialize this liquidity pool, the initial share is equal to the max value between currency amounts
					let initial_share: T::Share = sp_std::cmp::max(max_currency_amount_left, max_currency_amount_right).unique_saturated_into();

					(max_currency_amount_left, max_currency_amount_right, initial_share)
				} else {
					let (left_currency_pool, right_currency_pool): (Balance, Balance) = Self::liquidity_pool(pair_id);
					let left_price = Price::checked_from_rational(right_currency_pool, left_currency_pool).unwrap_or_default();
					let input_left_price = Price::checked_from_rational(max_currency_amount_right, max_currency_amount_left).unwrap_or_default();

					if input_left_price <= left_price {
						// max_currency_amount_left may be too much, calculate the actual left currency amount
						let base_left_price = Price::checked_from_rational(left_currency_pool, right_currency_pool).unwrap_or_default();
						let left_currency_amount = base_left_price.saturating_mul_int(max_currency_amount_right);
						let share = Ratio::checked_from_rational(left_currency_amount, left_currency_pool)
							.and_then(|n| n.checked_mul_int(total_shares))
							.unwrap_or_default();
						(left_currency_amount, max_currency_amount_right, share)
					} else {
						// max_currency_amount_right is too much, calculate the actual right currency amount
						let right_currency_amount = left_price.saturating_mul_int(max_currency_amount_left);
						let share = Ratio::checked_from_rational(right_currency_amount, right_currency_pool)
							.and_then(|n| n.checked_mul_int(total_shares))
							.unwrap_or_default();
						(max_currency_amount_left, right_currency_amount, share)
					}
				};

				ensure!(
					!share_increment.is_zero() && !left_currency_increment.is_zero() && !right_currency_increment.is_zero(),
					Error::<T>::InvalidLiquidityIncrement,
				);

        let sub_account = Self::sub_account_id(currency_id_left, currency_id_right);
        T::Currency::transfer(currency_id_left, &who, &sub_account, left_currency_increment)?;
				T::Currency::transfer(currency_id_right, &who, &sub_account, right_currency_increment)?;

				<TotalShares<T>>::try_mutate(pair_id, |total_shares| -> DispatchResult {
					*total_shares = total_shares.checked_add(&share_increment).ok_or(Error::<T>::SharesOverflow)?;
					Ok(())
				})?;
				<Shares<T>>::mutate(pair_id, &who, |share|
					*share = share.checked_add(&share_increment).expect("share cannot overflow if `total_shares` doesn't; qed")
				);
				LiquidityPool::mutate(pair_id, |(left, right)| {
					*left = left.saturating_add(left_currency_increment);
					*right = right.saturating_add(right_currency_increment);
				});
				T::OnAddLiquidity::happened(&(who.clone(), currency_id_left, currency_id_right, share_increment));

				Self::deposit_event(RawEvent::AddLiquidity(
					who,
					currency_id_left,
					currency_id_right,
					left_currency_increment,
					right_currency_increment,
					share_increment,
				));
				Ok(())
			})?;
		}

    #[weight = 248 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(11, 9)]
    pub fn withdraw_liquidity(origin,
                              currency_id_first: CurrencyId,
                              currency_id_second: CurrencyId,
                              #[compact] remove_share: T::Share) {
			ensure!(currency_id_first != currency_id_second, Error::<T>::InvalidCurrencyPair);
      with_transaction_result(|| {
        let who = ensure_signed(origin)?;
        if remove_share.is_zero() { return Ok(()); }

        let pair_id = Self::get_pair_key(&currency_id_first, &currency_id_second);
				ensure!(
					LiquidityPool::contains_key(pair_id),
					Error::<T>::InvalidCurrencyPair,
				);

        //
        // normalize currency pair, smaller at the left side
        let (currency_id_left, currency_id_right) = if currency_id_first < currency_id_second {
          (currency_id_first, currency_id_second)
        } else {
          (currency_id_second, currency_id_first)
        };

        let (other_currency_pool, base_currency_pool): (Balance, Balance) = Self::liquidity_pool(pair_id);

        let proportion = Ratio::checked_from_rational(remove_share, Self::total_shares(pair_id)).unwrap_or_default();
        let withdraw_other_currency_amount = proportion.saturating_mul_int(other_currency_pool);
        let withdraw_base_currency_amount = proportion.saturating_mul_int(base_currency_pool);

        let sub_account = Self::sub_account_id(currency_id_left, currency_id_right);
        T::Currency::transfer(currency_id_left, &sub_account, &who, withdraw_other_currency_amount)?;
        T::Currency::transfer(currency_id_right, &sub_account, &who, withdraw_base_currency_amount)?;

        <Shares<T>>::try_mutate(pair_id, &who, |share| -> DispatchResult {
          *share = share.checked_sub(&remove_share).ok_or(Error::<T>::ShareNotEnough)?;
          Ok(())
        })?;
        <TotalShares<T>>::mutate(pair_id, |share|
                                 *share = share.checked_sub(&remove_share).expect("total share cannot underflow if share doesn't; qed")
        );
        LiquidityPool::mutate(pair_id, |(other, base)| {
          *other = other.saturating_sub(withdraw_other_currency_amount);
          *base = base.saturating_sub(withdraw_base_currency_amount);
        });
        T::OnRemoveLiquidity::happened(&(who.clone(), currency_id_left, currency_id_right, remove_share));

        Self::deposit_event(RawEvent::WithdrawLiquidity(
          who,
          currency_id_left,
          currency_id_right,
          withdraw_other_currency_amount,
          withdraw_base_currency_amount,
          remove_share,
        ));
        Ok(())
			})?;
		}

	  #[weight = 200 * WEIGHT_PER_MICROS + T::DbWeight::get().reads_writes(9, 6)]
		pub fn basic_swap_currency(
			origin,
			supply_currency_id: CurrencyId,
			#[compact] supply_amount: Balance,
			target_currency_id: CurrencyId,
			#[compact] acceptable_target_amount: Balance,
		) {
			with_transaction_result(|| {
				let who = ensure_signed(origin)?;
				Self::basic_swap(&who, supply_currency_id, supply_amount, target_currency_id, acceptable_target_amount)?;
				Ok(())
			})?;
		}
  }
}

impl<T: Trait> Module<T> {
  /// generate the pair key from two currency ids.
  /// currency ids are sorted by asc order.
  /// the pair key was a u64 number, whose first 32bits is smaller currency id.
  /// the second 32bits is the greater currency id.
  pub fn get_pair_key(first: &CurrencyId, second: &CurrencyId) -> PairKey {
    let (left, right) = if first < second {
       (*first as u32, *second as u32)
    } else {
       (*second as u32, *first as u32)
    };
    let mut bytes = [0; 8];
    let numbers = [left, right];
    // ensure use little endian encoding
    LittleEndian::write_u32_into(&numbers, &mut bytes);
    LittleEndian::read_u64(&bytes)
  }

  pub fn pair_key_to_ids(pair_key: PairKey) -> Option<(CurrencyId, CurrencyId)> {
    let mut bytes = [0; 8];
    let numbers = [pair_key];
    // ensure use little endian encoding
    LittleEndian::write_u64_into(&numbers, &mut bytes);
    let left_id = LittleEndian::read_u32(&bytes[0 .. 4]);
    let right_id = LittleEndian::read_u32(&bytes[4 .. 8]);
    match (FromPrimitive::from_u32(left_id), FromPrimitive::from_u32(right_id)) {
      (Some(left), Some(right)) => Some((left, right)),
       _ => {
         debug::warn!("invalid pair ids: {:?}", pair_key);
         None
       },
    }
  }

  /// generate the sub account id from two currencies pair.
  /// the sub account is generated from the pair key.
	pub fn sub_account_id(first: CurrencyId, right: CurrencyId) -> T::AccountId {
    let pair_key = Self::get_pair_key(&first, &right);
		T::ModuleId::get().into_sub_account(pair_key)
  }

  /// get pool info from a currency pair
  /// pool info matches the input order
  pub fn get_pool_info(first: CurrencyId, second: CurrencyId) -> Result<(Balance, Balance), DispatchError> {
    let pair_id = Self::get_pair_key(&first, &second);
    if !LiquidityPool::contains_key(pair_id) {
      return Err(Error::<T>::InvalidCurrencyPair.into());
    };

    let (balance_left, balance_right) = Self::liquidity_pool(pair_id);
    if first < second {
      Ok((balance_left, balance_right))
    } else {
      Ok((balance_right, balance_left))
    }
  }

  fn calculate_swap_target_amount(
		supply_pool: Balance,
		target_pool: Balance,
		supply_amount: Balance,
		fee_rate: Rate,
	) -> Balance {
		if supply_amount.is_zero() {
			Zero::zero()
		} else {
			// new_target_pool = supply_pool * target_pool / (supply_amount + supply_pool)
			let new_target_pool = supply_pool
				.checked_add(supply_amount)
				.and_then(|n| Ratio::checked_from_rational(supply_pool, n))
				.and_then(|n| n.checked_mul_int(target_pool))
				.unwrap_or_default();

			if new_target_pool.is_zero() {
				Zero::zero()
			} else {
				// target_amount = (target_pool - new_target_pool) * (1 - fee_rate)
				target_pool
					.checked_sub(new_target_pool)
					.and_then(|n| Rate::one().saturating_sub(fee_rate).checked_mul_int(n))
					.unwrap_or_default()
			}
		}
	}

  /// Calculate how much supply token needed for swap specific target amount.
	fn calculate_swap_supply_amount(
		supply_pool: Balance,
		target_pool: Balance,
		target_amount: Balance,
		fee_rate: Rate,
	) -> Balance {
		if target_amount.is_zero() {
			Zero::zero()
		} else {
			// new_target_pool = target_pool - target_amount / (1 - fee_rate)
			let new_target_pool = Rate::one()
				.saturating_sub(fee_rate)
				.reciprocal()
				.and_then(|n| n.checked_add(&Ratio::from_inner(1))) // add 1 to result in order to correct the possible losses caused by remainder discarding in internal
				// division calculation
				.and_then(|n| n.checked_mul_int(target_amount))
				// add 1 to result in order to correct the possible losses caused by remainder discarding in internal
				// division calculation
				.and_then(|n| n.checked_add(Balance::one()))
				.and_then(|n| target_pool.checked_sub(n))
				.unwrap_or_default();

			if new_target_pool.is_zero() {
				Zero::zero()
			} else {
				// supply_amount = target_pool * supply_pool / new_target_pool - supply_pool
				Ratio::checked_from_rational(target_pool, new_target_pool)
					.and_then(|n| n.checked_add(&Ratio::from_inner(1))) // add 1 to result in order to correct the possible losses caused by remainder discarding in
					// internal division calculation
					.and_then(|n| n.checked_mul_int(supply_pool))
					.and_then(|n| n.checked_add(Balance::one())) // add 1 to result in order to correct the possible losses caused by remainder discarding in
					// internal division calculation
					.and_then(|n| n.checked_sub(supply_pool))
					.unwrap_or_default()
			}
		}
	}

  // direct swap two currencies
	fn basic_swap(
		who: &T::AccountId,
		from_currency_id: CurrencyId,
    from_currency_amount: Balance,
    target_currency_id: CurrencyId,
		acceptable_target_currency_amount: Balance,
	) -> sp_std::result::Result<Balance, DispatchError> {
		let (from_currency_pool, target_currency_pool) = Self::get_pool_info(from_currency_id, target_currency_id)?;
		let target_currency_amount = Self::calculate_swap_target_amount(
			from_currency_pool,
			target_currency_pool,
			from_currency_amount,
			T::GetExchangeFee::get(),
		);

		 // ensure the amount can get is not 0 and >= minium acceptable
		 ensure!(
		 	!target_currency_amount.is_zero() && target_currency_amount >= acceptable_target_currency_amount,
		 	Error::<T>::UnacceptablePrice,
		 );

     let pair_id = Self::get_pair_key(&from_currency_id, &target_currency_id);
     let sub_account = Self::sub_account_id(from_currency_id, target_currency_id);
		 //// transfer token between account and dex and update liquidity pool
		 T::Currency::transfer(from_currency_id, who, &sub_account, from_currency_amount)?;
		 T::Currency::transfer(target_currency_id, &sub_account, who, target_currency_amount)?;

		 LiquidityPool::mutate(pair_id, |(mut left, mut right)| {
       // update pool info
       // note: pool info are ordered, so we need to check the
       // order of the from currency and target currency
       let (from, target)= if from_currency_id < target_currency_id {
         (&mut left, &mut right)
       } else {
         (&mut right, &mut left)
       };

		 	 *from = from.saturating_add(from_currency_amount);
       *target = target.saturating_sub(target_currency_amount);
     });

		 Ok(target_currency_amount)
	}

  pub fn get_existing_currency_pairs() ->
    (vec::Vec<(CurrencyId, CurrencyId)>, btree_map::BTreeMap<PairKey, PoolInfo>) {
      let mut valid_info =  LiquidityPool::iter()
        .map(|(pair_key, pool_info)| (Self::pair_key_to_ids(pair_key), pair_key, pool_info))
        .filter(|(id, _, _)| id.is_some())
        .map(|(id, pk, info)| (id.unwrap(), pk, info));

      let currency_pairs = valid_info.by_ref().map(|(id, _, _)| id.clone()).collect();
      let pool_info = valid_info.map(|(_, pk, info)| (pk, info)).collect();

      (currency_pairs, pool_info)
    }

  pub fn build_currency_map(
    currency_pairs: &vec::Vec<(CurrencyId, CurrencyId)>)
    -> btree_map::BTreeMap<CurrencyId, vec::Vec<CurrencyId>>{
    let mut currency_data = btree_map::BTreeMap::<CurrencyId, vec::Vec<CurrencyId>>::new();
    for (currency_left, currency_right) in currency_pairs {
      if let Some(items_left) = currency_data.get_mut(&currency_left) {
        items_left.push(currency_right.clone());
      } else {
        currency_data.insert(currency_left.clone(), vec![currency_right.clone()]);
      };

      if let Some(items_right) = currency_data.get_mut(&currency_right) {
        items_right.push(currency_left.clone());
      } else {
        currency_data.insert(currency_right.clone(), vec![currency_left.clone()]);
      }
    }

    currency_data
  }

  // get the minimum amount of supply currency needed for the target currency
  // amount return 0 means cannot exchange
  // and the route info for the exchange
  pub fn get_supply_amount_needed(
    supply_currency_id: CurrencyId,
    target_currency_id: CurrencyId,
    target_currency_amount: Balance,
  ) -> (Balance, sp_std::vec::Vec<CurrencyId>) {
    if supply_currency_id == target_currency_id {
      // it doesn't make sense to exchange the same currency
      return (Zero::zero(), vec![]);
    }

    let pair_id = Self::get_pair_key(&supply_currency_id, &target_currency_id);

    let fee_rate = T::GetExchangeFee::get();

    if LiquidityPool::contains_key(pair_id) {
      // pool exists for the two currencies, use the pool directly
      let (other_currency_pool, base_currency_pool) = Self::liquidity_pool(pair_id);
      let amount = Self::calculate_swap_supply_amount(
        other_currency_pool,
        base_currency_pool,
        target_currency_amount,
        fee_rate,
      );
      return (amount, vec![target_currency_id]);
    }

    let (currency_pair, pool_info) = Self::get_existing_currency_pairs();
    let currency_map = Self::build_currency_map(&currency_pair);
    // find a reverse route from target to supply
    // as we need to caculate the cost reversely
    let routes = simple_graph::find_all_routes(
      &target_currency_id, &supply_currency_id,
      |currency| currency_map.get(&currency).unwrap_or(&vec![]).to_vec(), 6);

    debug::info!("got {:?} routes for currency: {:?}, target: {:?}", routes.len(), supply_currency_id, target_currency_id);

    // TODO: find a best route to do the exchange
    panic!("not implemented")
	}
}
