// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::NullessByteStr;

use super::arrays::{
    AccessVectorRules, ConditionalNodes, Context, DeprecatedFilenameTransitions,
    FilenameTransitionList, FilenameTransitions, FsUses, GenericFsContexts, IPv6Nodes,
    InfinitiBandEndPorts, InfinitiBandPartitionKeys, InitialSids, NamedContextPairs, Nodes, Ports,
    RangeTransitions, RoleAllow, RoleAllows, RoleTransition, RoleTransitions, SimpleArray,
    MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY, XPERMS_TYPE_IOCTL_PREFIXES,
    XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES,
};
use super::error::{ParseError, ValidateError};
use super::extensible_bitmap::ExtensibleBitmap;
use super::metadata::{Config, Counts, HandleUnknown, Magic, PolicyVersion, Signature};
use super::parser::ParseStrategy;
use super::security_context::{Level, SecurityContext};
use super::symbols::{
    Category, Class, Classes, CommonSymbol, CommonSymbols, ConditionalBoolean, MlsLevel, Role,
    Sensitivity, SymbolList, Type, User,
};
use super::{
    AccessDecision, AccessVector, CategoryId, ClassId, IoctlAccessDecision, Parse, RoleId,
    SensitivityId, TypeId, UserId, Validate, XpermsBitmap, SELINUX_AVD_FLAGS_PERMISSIVE,
};

use anyhow::Context as _;
use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use zerocopy::little_endian as le;

/// A parsed binary policy.
#[derive(Debug)]
pub struct ParsedPolicy<PS: ParseStrategy> {
    /// A distinctive number that acts as a binary format-specific header for SELinux binary policy
    /// files.
    magic: PS::Output<Magic>,
    /// A length-encoded string, "SE Linux", which identifies this policy as an SE Linux policy.
    signature: Signature<PS>,
    /// The policy format version number. Different version may support different policy features.
    policy_version: PS::Output<PolicyVersion>,
    /// Whole-policy configuration, such as how to handle queries against unknown classes.
    config: Config<PS>,
    /// High-level counts of subsequent policy elements.
    counts: PS::Output<Counts>,
    policy_capabilities: ExtensibleBitmap<PS>,
    permissive_map: ExtensibleBitmap<PS>,
    /// Common permissions that can be mixed in to classes.
    common_symbols: SymbolList<PS, CommonSymbol<PS>>,
    /// The set of classes referenced by this policy.
    classes: SymbolList<PS, Class<PS>>,
    /// The set of roles referenced by this policy.
    roles: SymbolList<PS, Role<PS>>,
    /// The set of types referenced by this policy.
    types: SymbolList<PS, Type<PS>>,
    /// The set of users referenced by this policy.
    users: SymbolList<PS, User<PS>>,
    /// The set of dynamically adjustable booleans referenced by this policy.
    conditional_booleans: SymbolList<PS, ConditionalBoolean<PS>>,
    /// The set of sensitivity levels referenced by this policy.
    sensitivities: SymbolList<PS, Sensitivity<PS>>,
    /// The set of categories referenced by this policy.
    categories: SymbolList<PS, Category<PS>>,
    /// The set of access vector rules referenced by this policy.
    access_vector_rules: SimpleArray<PS, AccessVectorRules<PS>>,
    conditional_lists: SimpleArray<PS, ConditionalNodes<PS>>,
    /// The set of role transitions to apply when instantiating new objects.
    role_transitions: RoleTransitions<PS>,
    /// The set of role transitions allowed by policy.
    role_allowlist: RoleAllows<PS>,
    filename_transition_list: FilenameTransitionList<PS>,
    initial_sids: SimpleArray<PS, InitialSids<PS>>,
    filesystems: SimpleArray<PS, NamedContextPairs<PS>>,
    ports: SimpleArray<PS, Ports<PS>>,
    network_interfaces: SimpleArray<PS, NamedContextPairs<PS>>,
    nodes: SimpleArray<PS, Nodes<PS>>,
    fs_uses: SimpleArray<PS, FsUses<PS>>,
    ipv6_nodes: SimpleArray<PS, IPv6Nodes<PS>>,
    infinitiband_partition_keys: Option<SimpleArray<PS, InfinitiBandPartitionKeys<PS>>>,
    infinitiband_end_ports: Option<SimpleArray<PS, InfinitiBandEndPorts<PS>>>,
    /// A set of labeling statements to apply to given filesystems and/or their subdirectories.
    /// Corresponds to the `genfscon` labeling statement in the policy.
    generic_fs_contexts: SimpleArray<PS, GenericFsContexts<PS>>,
    range_transitions: SimpleArray<PS, RangeTransitions<PS>>,
    /// Extensible bitmaps that encode associations between types and attributes.
    attribute_maps: Vec<ExtensibleBitmap<PS>>,
}

impl<PS: ParseStrategy> ParsedPolicy<PS> {
    /// The policy version stored in the underlying binary policy.
    pub fn policy_version(&self) -> u32 {
        PS::deref(&self.policy_version).policy_version()
    }

    /// The way "unknown" policy decisions should be handed according to the underlying binary
    /// policy.
    pub fn handle_unknown(&self) -> HandleUnknown {
        self.config.handle_unknown()
    }

    /// Computes the access granted to `source_type` on `target_type`, for the specified
    /// `target_class`. The result is a set of access vectors with bits set for each
    /// `target_class` permission, describing which permissions are allowed, and
    /// which should have access checks audit-logged when denied, or allowed.
    ///
    /// An [`AccessDecision`] is accumulated, starting from no permissions to be granted,
    /// nor audit-logged if allowed, and all permissions to be audit-logged if denied.
    /// Permissions that are explicitly `allow`ed, but that are subject to unsatisfied
    /// constraints, are removed from the allowed set. Matching policy statements then
    /// add permissions to the granted & audit-allow sets, or remove them from the
    /// audit-deny set.
    pub(super) fn compute_access_decision(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class<PS>,
    ) -> AccessDecision {
        let mut access_decision = self.compute_explicitly_allowed(
            source_context.type_(),
            target_context.type_(),
            target_class,
        );
        access_decision.allow -=
            self.compute_denied_by_constraints(source_context, target_context, target_class);
        access_decision
    }

    /// Computes the access granted to `source_type` on `target_type`, for the specified
    /// `target_class`. The result is a set of access vectors with bits set for each
    /// `target_class` permission, describing which permissions are explicitly allowed,
    /// and which should have access checks audit-logged when denied, or allowed.
    pub(super) fn compute_explicitly_allowed(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        target_class: &Class<PS>,
    ) -> AccessDecision {
        let target_class_id = target_class.id();

        let mut computed_access_vector = AccessVector::NONE;
        let mut computed_audit_allow = AccessVector::NONE;
        let mut computed_audit_deny = AccessVector::ALL;

        for access_vector_rule in self.access_vector_rules.data.iter() {
            // Ignore `access_vector_rule` entries not relayed to "allow" or
            // audit statements.
            //
            // TODO: https://fxbug.dev/379657220 - Can an `access_vector_rule`
            // entry express e.g. both "allow" and "auditallow" at the same
            // time?
            if !access_vector_rule.is_allow()
                && !access_vector_rule.is_auditallow()
                && !access_vector_rule.is_dontaudit()
            {
                continue;
            }

            // Concern ourselves only with `allow [source-type] [target-type]:[class] [...];`
            // policy statements where `[class]` matches `target_class_id`.
            if access_vector_rule.target_class() != target_class_id {
                continue;
            }

            // Note: Perform bitmap lookups last: they are the most expensive comparison operation.

            // Note: Type ids start at 1, but are 0-indexed in bitmaps: hence the `type - 1` bitmap
            // lookups below.

            // Concern ourselves only with `allow [source-type] [...];` policy statements where
            // `[source-type]` is associated with `source_type_id`.
            let source_attribute_bitmap: &ExtensibleBitmap<PS> =
                &self.attribute_maps[(source_type.0.get() - 1) as usize];
            if !source_attribute_bitmap.is_set(access_vector_rule.source_type().0.get() - 1) {
                continue;
            }

            // Concern ourselves only with `allow [source-type] [target-type][...];` policy
            // statements where `[target-type]` is associated with `target_type_id`.
            let target_attribute_bitmap: &ExtensibleBitmap<PS> =
                &self.attribute_maps[(target_type.0.get() - 1) as usize];
            if !target_attribute_bitmap.is_set(access_vector_rule.target_type().0.get() - 1) {
                continue;
            }

            // Multiple attributes may be associated with source/target types. Accumulate
            // explicitly allowed permissions into `computed_access_vector`.
            if let Some(access_vector) = access_vector_rule.access_vector() {
                if access_vector_rule.is_allow() {
                    // `access_vector` has bits set for each permission allowed by this rule.
                    computed_access_vector |= access_vector;
                } else if access_vector_rule.is_auditallow() {
                    // `access_vector` has bits set for each permission to audit when allowed.
                    computed_audit_allow |= access_vector;
                } else if access_vector_rule.is_dontaudit() {
                    // `access_vector` has bits cleared for each permission not to audit on denial.
                    computed_audit_deny &= access_vector;
                }
            }
        }

        // TODO: https://fxbug.dev/362706116 - Collate the auditallow & auditdeny sets.
        let mut flags = 0;
        if self.permissive_types().is_set(source_type.0.get()) {
            flags |= SELINUX_AVD_FLAGS_PERMISSIVE;
        }
        AccessDecision {
            allow: computed_access_vector,
            auditallow: computed_audit_allow,
            auditdeny: computed_audit_deny,
            flags,
            todo_bug: None,
        }
    }

    /// A permission is denied if it matches at least one unsatisfied constraint.
    fn compute_denied_by_constraints(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class<PS>,
    ) -> AccessVector {
        let mut denied = AccessVector::NONE;
        for constraint in target_class.constraints().iter() {
            match constraint.constraint_expr().evaluate(source_context, target_context) {
                Err(err) => {
                    unreachable!("validated constraint expression failed to evaluate: {:?}", err)
                }
                Ok(false) => denied |= constraint.access_vector(),
                Ok(true) => {}
            }
        }
        denied
    }

    /// Computes the ioctl extended permissions that should be allowed, audited when allowed, and
    /// audited when denied, for a given source context, target context, target class, and ioctl
    /// prefix byte.
    ///
    /// If there is an `allowxperm` rule for a particular source, target, and class, then only the
    /// named xperms should be allowed for that tuple. If there is no such `allowxperm` rule, then
    /// all xperms should be allowed for that tuple. (In both cases, the allow is conditional on the
    /// `ioctl` permission being allowed, but that should be checked separately before calling this
    /// function.)
    pub(super) fn compute_ioctl_access_decision(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class<PS>,
        ioctl_prefix: u8,
    ) -> IoctlAccessDecision {
        let target_class_id = target_class.id();

        let mut explicit_allow: Option<XpermsBitmap> = None;
        let mut auditallow = XpermsBitmap::NONE;
        let mut auditdeny = XpermsBitmap::ALL;

        for access_vector_rule in self.access_vector_rules.data.iter() {
            if !access_vector_rule.is_allowxperm()
                && !access_vector_rule.is_auditallowxperm()
                && !access_vector_rule.is_dontauditxperm()
            {
                continue;
            }
            if access_vector_rule.target_class() != target_class_id {
                continue;
            }
            let source_attribute_bitmap: &ExtensibleBitmap<PS> =
                &self.attribute_maps[(source_context.type_().0.get() - 1) as usize];
            if !source_attribute_bitmap.is_set(access_vector_rule.source_type().0.get() - 1) {
                continue;
            }
            let target_attribute_bitmap: &ExtensibleBitmap<PS> =
                &self.attribute_maps[(target_context.type_().0.get() - 1) as usize];
            if !target_attribute_bitmap.is_set(access_vector_rule.target_type().0.get() - 1) {
                continue;
            }

            if let Some(xperms) = access_vector_rule.extended_permissions() {
                // Only filter ioctls if there is at least one `allowxperm` rule for any ioctl
                // prefix.
                if access_vector_rule.is_allowxperm() {
                    explicit_allow.get_or_insert(XpermsBitmap::NONE);
                }
                // If the rule applies to ioctls with prefix `ioctl_prefix`, get a bitmap
                // of the ioctl postfixes named in the rule.
                let bitmap_if_prefix_matches = match xperms.xperms_type {
                    XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES => (xperms.xperms_optional_prefix
                        == ioctl_prefix)
                        .then_some(&xperms.xperms_bitmap),
                    XPERMS_TYPE_IOCTL_PREFIXES => {
                        xperms.xperms_bitmap.contains(ioctl_prefix).then_some(&XpermsBitmap::ALL)
                    }
                    _ => unreachable!("invalid xperms_type in validated ExtendedPermissions"),
                };
                let Some(xperms_bitmap) = bitmap_if_prefix_matches else {
                    continue;
                };
                if access_vector_rule.is_allowxperm() {
                    (*explicit_allow.get_or_insert(XpermsBitmap::NONE)) |= xperms_bitmap;
                }
                if access_vector_rule.is_auditallowxperm() {
                    auditallow |= xperms_bitmap;
                }
                if access_vector_rule.is_dontauditxperm() {
                    auditdeny -= xperms_bitmap;
                }
            }
        }
        let allow = explicit_allow.unwrap_or(XpermsBitmap::ALL);
        IoctlAccessDecision { allow, auditallow, auditdeny }
    }

    /// Returns the policy entry for the specified initial Security Context.
    pub(super) fn initial_context(&self, id: crate::InitialSid) -> &Context<PS> {
        let id = le::U32::from(id as u32);
        // [`InitialSids`] validates that all `InitialSid` values are defined by the policy.
        &self.initial_sids.data.iter().find(|initial| initial.id() == id).unwrap().context()
    }

    /// Returns the `User` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn user(&self, id: UserId) -> &User<PS> {
        self.users.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named user, if present in the policy.
    pub(super) fn user_by_name(&self, name: &str) -> Option<&User<PS>> {
        self.users.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the `Role` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn role(&self, id: RoleId) -> &Role<PS> {
        self.roles.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named role, if present in the policy.
    pub(super) fn role_by_name(&self, name: &str) -> Option<&Role<PS>> {
        self.roles.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the `Type` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn type_(&self, id: TypeId) -> &Type<PS> {
        self.types.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named type, if present in the policy.
    pub(super) fn type_by_name(&self, name: &str) -> Option<&Type<PS>> {
        self.types.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the extensible bitmap describing the set of types/domains for which permission
    /// checks are permissive.
    pub(super) fn permissive_types(&self) -> &ExtensibleBitmap<PS> {
        &self.permissive_map
    }

    /// Returns the `Sensitivity` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn sensitivity(&self, id: SensitivityId) -> &Sensitivity<PS> {
        self.sensitivities.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named sensitivity level, if present in the policy.
    pub(super) fn sensitivity_by_name(&self, name: &str) -> Option<&Sensitivity<PS>> {
        self.sensitivities.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the `Category` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn category(&self, id: CategoryId) -> &Category<PS> {
        self.categories.data.iter().find(|y| y.id() == id).unwrap()
    }

    /// Returns the named category, if present in the policy.
    pub(super) fn category_by_name(&self, name: &str) -> Option<&Category<PS>> {
        self.categories.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    pub(super) fn classes(&self) -> &Classes<PS> {
        &self.classes.data
    }

    pub(super) fn common_symbols(&self) -> &CommonSymbols<PS> {
        &self.common_symbols.data
    }

    pub(super) fn conditional_booleans(&self) -> &Vec<ConditionalBoolean<PS>> {
        &self.conditional_booleans.data
    }

    pub(super) fn fs_uses(&self) -> &FsUses<PS> {
        &self.fs_uses.data
    }

    pub(super) fn generic_fs_contexts(&self) -> &GenericFsContexts<PS> {
        &self.generic_fs_contexts.data
    }

    #[allow(dead_code)]
    // TODO(http://b/334968228): fn to be used again when checking role allow rules separately from
    // SID calculation.
    pub(super) fn role_allowlist(&self) -> &[RoleAllow] {
        PS::deref_slice(&self.role_allowlist.data)
    }

    pub(super) fn role_transitions(&self) -> &[RoleTransition] {
        PS::deref_slice(&self.role_transitions.data)
    }

    pub(super) fn range_transitions(&self) -> &RangeTransitions<PS> {
        &self.range_transitions.data
    }

    pub(super) fn access_vector_rules(&self) -> &AccessVectorRules<PS> {
        &self.access_vector_rules.data
    }

    pub(super) fn compute_filename_transition(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: ClassId,
        name: NullessByteStr<'_>,
    ) -> Option<TypeId> {
        match &self.filename_transition_list {
            FilenameTransitionList::PolicyVersionGeq33(list) => {
                let entry = list.data.iter().find(|transition| {
                    transition.target_type() == target_type
                        && transition.target_class() == class
                        && transition.name_bytes() == name.as_bytes()
                })?;
                entry
                    .outputs()
                    .iter()
                    .find(|entry| entry.has_source_type(source_type))
                    .map(|x| x.out_type())
            }
            FilenameTransitionList::PolicyVersionLeq32(list) => list
                .data
                .iter()
                .find(|transition| {
                    transition.target_class() == class
                        && transition.target_type() == target_type
                        && transition.source_type() == source_type
                        && transition.name_bytes() == name.as_bytes()
                })
                .map(|x| x.out_type()),
        }
    }

    // Validate an MLS range statement against sets of defined sensitivity and category
    // IDs:
    // - Verify that all sensitivity and category IDs referenced in the MLS levels are
    //   defined.
    // - Verify that the range is internally consistent; i.e., the high level (if any)
    //   dominates the low level.
    fn validate_mls_range(
        &self,
        low_level: &MlsLevel<PS>,
        high_level: &Option<MlsLevel<PS>>,
        sensitivity_ids: &HashSet<SensitivityId>,
        category_ids: &HashSet<CategoryId>,
    ) -> Result<(), anyhow::Error> {
        validate_id(sensitivity_ids, low_level.sensitivity(), "sensitivity")?;
        for id in low_level.category_ids() {
            validate_id(category_ids, id, "category")?;
        }
        if let Some(high) = high_level {
            validate_id(sensitivity_ids, high.sensitivity(), "sensitivity")?;
            for id in high.category_ids() {
                validate_id(category_ids, id, "category")?;
            }
            if !high.dominates(low_level) {
                return Err(ValidateError::InvalidMlsRange {
                    low: low_level.serialize(self).into(),
                    high: high.serialize(self).into(),
                }
                .into());
            }
        }
        Ok(())
    }
}

impl<PS: ParseStrategy> ParsedPolicy<PS>
where
    Self: Parse<PS>,
{
    /// Parses the binary policy stored in `bytes`. It is an error for `bytes` to have trailing
    /// bytes after policy parsing completes.
    pub(super) fn parse(bytes: PS) -> Result<(Self, PS::Input), anyhow::Error> {
        let (policy, tail) =
            <ParsedPolicy<PS> as Parse<PS>>::parse(bytes).map_err(Into::<anyhow::Error>::into)?;
        let num_bytes = tail.len();
        if num_bytes > 0 {
            return Err(ParseError::TrailingBytes { num_bytes }.into());
        }
        Ok((policy, tail.into_inner()))
    }
}

/// Parse a data structure from a prefix of a [`ParseStrategy`].
impl<PS: ParseStrategy> Parse<PS> for ParsedPolicy<PS>
where
    Signature<PS>: Parse<PS>,
    ExtensibleBitmap<PS>: Parse<PS>,
    SymbolList<PS, CommonSymbol<PS>>: Parse<PS>,
    SymbolList<PS, Class<PS>>: Parse<PS>,
    SymbolList<PS, Role<PS>>: Parse<PS>,
    SymbolList<PS, Type<PS>>: Parse<PS>,
    SymbolList<PS, User<PS>>: Parse<PS>,
    SymbolList<PS, ConditionalBoolean<PS>>: Parse<PS>,
    SymbolList<PS, Sensitivity<PS>>: Parse<PS>,
    SymbolList<PS, Category<PS>>: Parse<PS>,
    SimpleArray<PS, AccessVectorRules<PS>>: Parse<PS>,
    SimpleArray<PS, ConditionalNodes<PS>>: Parse<PS>,
    RoleTransitions<PS>: Parse<PS>,
    RoleAllows<PS>: Parse<PS>,
    SimpleArray<PS, FilenameTransitions<PS>>: Parse<PS>,
    SimpleArray<PS, DeprecatedFilenameTransitions<PS>>: Parse<PS>,
    SimpleArray<PS, InitialSids<PS>>: Parse<PS>,
    SimpleArray<PS, NamedContextPairs<PS>>: Parse<PS>,
    SimpleArray<PS, Ports<PS>>: Parse<PS>,
    SimpleArray<PS, NamedContextPairs<PS>>: Parse<PS>,
    SimpleArray<PS, Nodes<PS>>: Parse<PS>,
    SimpleArray<PS, FsUses<PS>>: Parse<PS>,
    SimpleArray<PS, IPv6Nodes<PS>>: Parse<PS>,
    SimpleArray<PS, InfinitiBandPartitionKeys<PS>>: Parse<PS>,
    SimpleArray<PS, InfinitiBandEndPorts<PS>>: Parse<PS>,
    SimpleArray<PS, GenericFsContexts<PS>>: Parse<PS>,
    SimpleArray<PS, RangeTransitions<PS>>: Parse<PS>,
{
    /// A [`Policy`] may add context to underlying [`ParseError`] values.
    type Error = anyhow::Error;

    /// Parses an entire binary policy.
    fn parse(bytes: PS) -> Result<(Self, PS), Self::Error> {
        let tail = bytes;

        let (magic, tail) = PS::parse::<Magic>(tail).context("parsing magic")?;

        let (signature, tail) = Signature::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing signature")?;

        let (policy_version, tail) =
            PS::parse::<PolicyVersion>(tail).context("parsing policy version")?;
        let policy_version_value = PS::deref(&policy_version).policy_version();

        let (config, tail) = Config::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing policy config")?;

        let (counts, tail) =
            PS::parse::<Counts>(tail).context("parsing high-level policy object counts")?;

        let (policy_capabilities, tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing policy capabilities")?;

        let (permissive_map, tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing permissive map")?;

        let (common_symbols, tail) = SymbolList::<PS, CommonSymbol<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing common symbols")?;

        let (classes, tail) = SymbolList::<PS, Class<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing classes")?;

        let (roles, tail) = SymbolList::<PS, Role<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing roles")?;

        let (types, tail) = SymbolList::<PS, Type<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing types")?;

        let (users, tail) = SymbolList::<PS, User<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing users")?;

        let (conditional_booleans, tail) = SymbolList::<PS, ConditionalBoolean<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing conditional booleans")?;

        let (sensitivities, tail) = SymbolList::<PS, Sensitivity<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing sensitivites")?;

        let (categories, tail) = SymbolList::<PS, Category<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing categories")?;

        let (access_vector_rules, tail) = SimpleArray::<PS, AccessVectorRules<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing access vector rules")?;

        let (conditional_lists, tail) = SimpleArray::<PS, ConditionalNodes<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing conditional lists")?;

        let (role_transitions, tail) = RoleTransitions::<PS>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing role transitions")?;

        let (role_allowlist, tail) = RoleAllows::<PS>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing role allow rules")?;

        let (filename_transition_list, tail) = if policy_version_value >= 33 {
            let (filename_transition_list, tail) =
                SimpleArray::<PS, FilenameTransitions<PS>>::parse(tail)
                    .map_err(Into::<anyhow::Error>::into)
                    .context("parsing standard filename transitions")?;
            (FilenameTransitionList::PolicyVersionGeq33(filename_transition_list), tail)
        } else {
            let (filename_transition_list, tail) =
                SimpleArray::<PS, DeprecatedFilenameTransitions<PS>>::parse(tail)
                    .map_err(Into::<anyhow::Error>::into)
                    .context("parsing deprecated filename transitions")?;
            (FilenameTransitionList::PolicyVersionLeq32(filename_transition_list), tail)
        };

        let (initial_sids, tail) = SimpleArray::<PS, InitialSids<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing initial sids")?;

        let (filesystems, tail) = SimpleArray::<PS, NamedContextPairs<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing filesystem contexts")?;

        let (ports, tail) = SimpleArray::<PS, Ports<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing ports")?;

        let (network_interfaces, tail) = SimpleArray::<PS, NamedContextPairs<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing network interfaces")?;

        let (nodes, tail) = SimpleArray::<PS, Nodes<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing nodes")?;

        let (fs_uses, tail) = SimpleArray::<PS, FsUses<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing fs uses")?;

        let (ipv6_nodes, tail) = SimpleArray::<PS, IPv6Nodes<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing ipv6 nodes")?;

        let (infinitiband_partition_keys, infinitiband_end_ports, tail) =
            if policy_version_value >= MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY {
                let (infinity_band_partition_keys, tail) =
                    SimpleArray::<PS, InfinitiBandPartitionKeys<PS>>::parse(tail)
                        .map_err(Into::<anyhow::Error>::into)
                        .context("parsing infiniti band partition keys")?;
                let (infinitiband_end_ports, tail) =
                    SimpleArray::<PS, InfinitiBandEndPorts<PS>>::parse(tail)
                        .map_err(Into::<anyhow::Error>::into)
                        .context("parsing infiniti band end ports")?;
                (Some(infinity_band_partition_keys), Some(infinitiband_end_ports), tail)
            } else {
                (None, None, tail)
            };

        let (generic_fs_contexts, tail) = SimpleArray::<PS, GenericFsContexts<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing generic filesystem contexts")?;

        let (range_transitions, tail) = SimpleArray::<PS, RangeTransitions<PS>>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing range transitions")?;

        let primary_names_count = PS::deref(&types.metadata).primary_names_count();
        let mut attribute_maps = Vec::with_capacity(primary_names_count as usize);
        let mut tail = tail;

        for i in 0..primary_names_count {
            let (item, next_tail) = ExtensibleBitmap::parse(tail)
                .map_err(Into::<anyhow::Error>::into)
                .with_context(|| format!("parsing {}th attribtue map", i))?;
            attribute_maps.push(item);
            tail = next_tail;
        }
        let tail = tail;
        let attribute_maps = attribute_maps;

        Ok((
            Self {
                magic,
                signature,
                policy_version,
                config,
                counts,
                policy_capabilities,
                permissive_map,
                common_symbols,
                classes,
                roles,
                types,
                users,
                conditional_booleans,
                sensitivities,
                categories,
                access_vector_rules,
                conditional_lists,
                role_transitions,
                role_allowlist,
                filename_transition_list,
                initial_sids,
                filesystems,
                ports,
                network_interfaces,
                nodes,
                fs_uses,
                ipv6_nodes,
                infinitiband_partition_keys,
                infinitiband_end_ports,
                generic_fs_contexts,
                range_transitions,
                attribute_maps,
            },
            tail,
        ))
    }
}

impl<PS: ParseStrategy> Validate for ParsedPolicy<PS> {
    /// A [`Policy`] may add context to underlying [`ValidateError`] values.
    type Error = anyhow::Error;

    fn validate(&self) -> Result<(), Self::Error> {
        PS::deref(&self.magic)
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating magic")?;
        self.signature
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating signature")?;
        PS::deref(&self.policy_version)
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating policy_version")?;
        self.config.validate().map_err(Into::<anyhow::Error>::into).context("validating config")?;
        PS::deref(&self.counts)
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating counts")?;
        self.policy_capabilities
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating policy_capabilities")?;
        self.permissive_map
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating permissive_map")?;
        self.common_symbols
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating common_symbols")?;
        self.classes
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating classes")?;
        self.roles.validate().map_err(Into::<anyhow::Error>::into).context("validating roles")?;
        self.types.validate().map_err(Into::<anyhow::Error>::into).context("validating types")?;
        self.users.validate().map_err(Into::<anyhow::Error>::into).context("validating users")?;
        self.conditional_booleans
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating conditional_booleans")?;
        self.sensitivities
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating sensitivities")?;
        self.categories
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating categories")?;
        self.access_vector_rules
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating access_vector_rules")?;
        self.conditional_lists
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating conditional_lists")?;
        self.role_transitions
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating role_transitions")?;
        self.role_allowlist
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating role_allowlist")?;
        self.filename_transition_list
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating filename_transition_list")?;
        self.initial_sids
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating initial_sids")?;
        self.filesystems
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating filesystems")?;
        self.ports.validate().map_err(Into::<anyhow::Error>::into).context("validating ports")?;
        self.network_interfaces
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating network_interfaces")?;
        self.nodes.validate().map_err(Into::<anyhow::Error>::into).context("validating nodes")?;
        self.fs_uses
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating fs_uses")?;
        self.ipv6_nodes
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating ipv6 nodes")?;
        self.infinitiband_partition_keys
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating infinitiband_partition_keys")?;
        self.infinitiband_end_ports
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating infinitiband_end_ports")?;
        self.generic_fs_contexts
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating generic_fs_contexts")?;
        self.range_transitions
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating range_transitions")?;
        self.attribute_maps
            .validate()
            .map_err(Into::<anyhow::Error>::into)
            .context("validating attribute_maps")?;

        // Collate the sets of user, role, type, sensitivity and category Ids.
        let user_ids: HashSet<UserId> = self.users.data.iter().map(|x| x.id()).collect();
        let role_ids: HashSet<RoleId> = self.roles.data.iter().map(|x| x.id()).collect();
        let type_ids: HashSet<TypeId> = self.types.data.iter().map(|x| x.id()).collect();
        let sensitivity_ids: HashSet<SensitivityId> =
            self.sensitivities.data.iter().map(|x| x.id()).collect();
        let category_ids: HashSet<CategoryId> =
            self.categories.data.iter().map(|x| x.id()).collect();

        // Validate that users use only defined sensitivities and categories, and that
        // each user's MLS levels are internally consistent (i.e., the high level
        // dominates the low level).
        for user in &self.users.data {
            self.validate_mls_range(
                user.mls_range().low(),
                user.mls_range().high(),
                &sensitivity_ids,
                &category_ids,
            )?;
        }

        // Validate that initial contexts use only defined user, role, type, etc Ids.
        // Check that all sensitivity and category IDs are defined and that MLS levels
        // are internally consistent.
        for initial_sid in &self.initial_sids.data {
            let context = initial_sid.context();
            validate_id(&user_ids, context.user_id(), "user")?;
            validate_id(&role_ids, context.role_id(), "role")?;
            validate_id(&type_ids, context.type_id(), "type")?;
            self.validate_mls_range(
                context.low_level(),
                context.high_level(),
                &sensitivity_ids,
                &category_ids,
            )?;
        }

        // Validate that contexts specified in filesystem labeling rules only use
        // policy-defined Ids for their fields. Check that MLS levels are internally
        // consistent.
        for fs_use in &self.fs_uses.data {
            let context = fs_use.context();
            validate_id(&user_ids, context.user_id(), "user")?;
            validate_id(&role_ids, context.role_id(), "role")?;
            validate_id(&type_ids, context.type_id(), "type")?;
            self.validate_mls_range(
                context.low_level(),
                context.high_level(),
                &sensitivity_ids,
                &category_ids,
            )?;
        }

        // Validate that roles output by role- transitions & allows are defined.
        for transition in PS::deref_slice(&self.role_transitions.data) {
            validate_id(&role_ids, transition.new_role(), "new_role")?;
        }
        for allow in PS::deref_slice(&self.role_allowlist.data) {
            validate_id(&role_ids, allow.new_role(), "new_role")?;
        }

        // Validate that types output by access vector rules are defined.
        for access_vector_rule in &self.access_vector_rules.data {
            if let Some(type_id) = access_vector_rule.new_type() {
                validate_id(&type_ids, type_id, "new_type")?;
            }
        }

        // Validate that constraints are well-formed by evaluating against
        // a source and target security context.
        let initial_context = SecurityContext::new_from_policy_context(
            self.initial_context(crate::InitialSid::Kernel),
        );
        for class in self.classes() {
            for constraint in class.constraints() {
                constraint
                    .constraint_expr()
                    .evaluate(&initial_context, &initial_context)
                    .map_err(Into::<anyhow::Error>::into)
                    .context("validating constraints")?;
            }
        }

        // To-do comments for cross-policy validations yet to be implemented go here.
        // TODO(b/356569876): Determine which "bounds" should be verified for correctness here.

        Ok(())
    }
}

fn validate_id<IdType: Debug + Eq + Hash>(
    id_set: &HashSet<IdType>,
    id: IdType,
    debug_kind: &'static str,
) -> Result<(), anyhow::Error> {
    if !id_set.contains(&id) {
        return Err(ValidateError::UnknownId { kind: debug_kind, id: format!("{:?}", id) }.into());
    }
    Ok(())
}
